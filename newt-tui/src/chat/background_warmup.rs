//! Background warm-up of the session's navigation and semantic indexes.
//!
//! #1387 builds where_is + usage + graph + project model once per session;
//! the semantic corpus is embedded off-thread so the first turn does not
//! block on it. Everything here is spawn/poll/finish plumbing over
//! [`super::BackgroundJob`] plus the language-pack resolution the indexes
//! are built from. Command execution against those indexes lives in
//! `super::navigation_execution`; the session loop stays in `super`.

use super::*;

/// #1387: build where_is + usage + graph + project model once per session.
///
/// Uses the same language-pack registry as `find` `category=source` so "code"
/// means one thing everywhere.
pub(super) fn ensure_nav_indexes(
    workspace: &str,
    cfg: &newt_core::Config,
    where_is_index: &mut Option<newt_core::WhereIsIndex>,
    nav_session: &mut newt_core::NavigatorSession,
    index_status: &newt_core::IndexStatus,
) {
    use newt_core::{gather_with_manifest, GatherCaps};
    let api_cfg = cfg
        .context
        .as_ref()
        .map(|context| context.api_surface.clone())
        .unwrap_or_default();
    let packs = resolved_language_packs(workspace, &api_cfg);
    let exts = newt_core::api_surface::source_extensions_for(&packs, None).unwrap_or_default();
    let (files, manifest) = gather_with_manifest(workspace, &exts, GatherCaps::default());
    let cuts_open = !manifest.cuts.is_empty();
    let id = index_status.index_id();
    if where_is_index.is_none() {
        *where_is_index = Some(newt_core::build_where_is_index(&files, &packs, &manifest));
    }
    if nav_session.usage.is_none() {
        nav_session.usage = Some(newt_core::UsageIndex::build(&files, cuts_open, &id));
    }
    if nav_session.graph.is_none() {
        nav_session.graph = Some(newt_core::GraphIndex::build(&files, cuts_open, &id));
    }
    if nav_session.project.is_none() {
        let root = std::path::Path::new(workspace);
        nav_session.project = newt_core::project_model::scan_project(
            root,
            &newt_core::project_model::builtin_project_packs(),
        );
    }
    nav_session.files = files;
    nav_session.ledger.set_index(id);
}

pub(super) struct SemanticIndexWarmup {
    pub(super) handle: tokio::task::JoinHandle<usize>,
    pub(super) job: BackgroundJob,
}

/// Iteration #4 of bug/steering-regressions: embedding the gathered corpus
/// through the in-process CPU embedder ran SYNCHRONOUSLY inside the first
/// agentic turn (`block_on(index_files…)`) — observed live as 40–80 minutes at
/// ~6.5 cores with the session apparently wedged between a tool result and the
/// next dispatch. Same defect class as the navigator warm-up join (iteration
/// #3), one layer down. Indexing now runs as a background task; retrieval
/// rides the lexical floor until the index is ready. Never trade turn
/// liveness for index completeness.
pub(super) fn spawn_semantic_indexing(
    rt: &tokio::runtime::Handle,
    files: Vec<(String, String)>,
    embedder: std::sync::Arc<dyn newt_core::Embedder>,
    index: std::sync::Arc<newt_core::SessionSemanticIndex>,
    on_failure: newt_core::OnEmbedFailure,
) -> SemanticIndexWarmup {
    let job = BackgroundJob::start("embedding repository for semantic retrieval");
    let completion = job.completion_guard();
    // Iteration #8: the embedded candle engine's forwards are SYNCHRONOUS
    // compute. Run on a plain async task they poll-block the runtime workers
    // themselves — observed live as total executor starvation (frozen pane,
    // zero network, every rt-worker pegged) MINUTES after iteration #4 moved
    // this off the turn. spawn_blocking confines the drive to one parked
    // blocking-pool thread; candle's internal parallelism is unaffected and
    // the async runtime stays responsive.
    let inner = rt.clone();
    let handle = rt.spawn_blocking(move || {
        let _completion = completion;
        inner.block_on(newt_core::index_files(
            &files,
            embedder.as_ref(),
            index.as_ref(),
            on_failure,
        ))
    });
    SemanticIndexWarmup { handle, job }
}

/// Adopt a FINISHED semantic-indexing warm-up (never blocks): returns the
/// chunk count once, for the completion notice. A still-running build is left
/// running; a panicked/aborted build is consumed silently (the lexical floor
/// already covers the gap).
pub(super) fn poll_semantic_indexing(
    rt: &tokio::runtime::Handle,
    warmup: &mut Option<SemanticIndexWarmup>,
) -> Option<usize> {
    let pending = warmup.take()?;
    if !pending.handle.is_finished() {
        *warmup = Some(pending);
        return None;
    }
    tokio::task::block_in_place(|| rt.block_on(pending.handle)).ok()
}

pub(super) type NavWarmupOutput = (Option<newt_core::WhereIsIndex>, newt_core::NavigatorSession);

pub(super) struct NavWarmup {
    pub(super) handle: tokio::task::JoinHandle<NavWarmupOutput>,
    pub(super) job: BackgroundJob,
}

impl NavWarmup {
    pub(super) fn abort(self) {
        self.handle.abort();
    }
}

pub(super) fn spawn_nav_warmup(
    rt: &tokio::runtime::Handle,
    workspace: &str,
    cfg: &newt_core::Config,
    index_status: &newt_core::IndexStatus,
) -> NavWarmup {
    let workspace = workspace.to_string();
    let cfg = cfg.clone();
    let index_status = index_status.clone();
    let job = BackgroundJob::start("indexing repository");
    let completion = job.completion_guard();
    let handle = rt.spawn_blocking(move || {
        let _completion = completion;
        let mut where_is = None;
        let mut nav = newt_core::NavigatorSession::default();
        ensure_nav_indexes(&workspace, &cfg, &mut where_is, &mut nav, &index_status);
        (where_is, nav)
    });
    NavWarmup { handle, job }
}

/// Adopt the background navigator warm-up ONLY if it has already finished.
///
/// bug/steering-regressions iteration #3: this used to `block_on` the join,
/// so the turn stalled for as long as the index build ran — observed live
/// twice (2026-07-27): 40+ minutes at ~6 cores on a corpus-heavy workspace,
/// the session apparently wedged (no output, no inference, no tool calls).
/// The navigator's own contract already degrades honestly without an index
/// (regex floor, `complete=false`), so a still-running warm-up now simply
/// keeps running: this turn uses the floor and a later turn adopts the
/// finished index. Never trade turn liveness for index completeness.
pub(super) fn finish_nav_warmup(
    rt: &tokio::runtime::Handle,
    warmup: &mut Option<NavWarmup>,
    where_is: &mut Option<newt_core::WhereIsIndex>,
    nav: &mut newt_core::NavigatorSession,
) {
    let Some(pending) = warmup.take() else {
        return;
    };
    if !pending.handle.is_finished() {
        *warmup = Some(pending);
        return;
    }
    if let Ok((warmed_where_is, warmed_nav)) =
        tokio::task::block_in_place(|| rt.block_on(pending.handle))
    {
        *where_is = warmed_where_is;
        *nav = warmed_nav;
    }
}

pub(super) fn resolved_language_packs(
    workspace: &str,
    api_cfg: &newt_core::config::ApiSurfaceConfig,
) -> Vec<newt_core::config::LanguagePack> {
    newt_core::api_surface::resolve_language_packs(std::path::Path::new(workspace), api_cfg)
}

pub(super) fn resolved_source_extensions(workspace: &str, cfg: &newt_core::Config) -> Vec<String> {
    let api_cfg = cfg
        .context
        .as_ref()
        .map(|context| context.api_surface.clone())
        .unwrap_or_default();
    let packs = resolved_language_packs(workspace, &api_cfg);
    newt_core::api_surface::source_extensions_for(&packs, None).unwrap_or_default()
}

#[cfg(test)]
#[path = "../chat_tests/background_warmup.rs"]
mod background_warmup_tests;
