use super::*;

/// Grounds the background task in a real source workspace: startup may run
/// concurrently, but the first consumer joins a complete structural index
/// rather than observing a partially built belief.
#[tokio::test(flavor = "multi_thread")]
async fn repository_navigator_warms_in_background_and_joins_complete() {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("main.rs"),
        "pub fn warm_marker() {}\n",
    )
    .unwrap();
    let workspace = workspace.path().to_string_lossy().into_owned();
    let rt = tokio::runtime::Handle::current();
    let mut warmup = spawn_nav_warmup(
        &rt,
        &workspace,
        &newt_core::Config::default(),
        &newt_core::IndexStatus::default(),
        true,
    );
    let job = warmup.as_ref().unwrap().job.clone();
    let mut where_is = None;
    let mut nav = newt_core::NavigatorSession::default();

    // Iteration #3 contract: adoption happens only once the build is done —
    // wait for the build to finish (the event; only the shared hang guard
    // bounds it), then adopt. A still-running warm-up is covered by
    // `unfinished_warmup_is_left_running_and_the_turn_degrades`. The former
    // 200 × 50 ms loop was a 10 s budget that a loaded box crossed.
    let deadline = std::time::Instant::now() + newt_core::test_guard::HANG_GUARD;
    while !warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the warm-up build never finished"
        );
        tokio::task::yield_now().await;
    }
    finish_nav_warmup(&rt, &mut warmup, &mut where_is, &mut nav);

    assert!(warmup.is_none(), "a finished warm-up must be adopted");
    assert!(
        !job.is_running(),
        "joining the warm-up must clear its generic liveness indicator"
    );
    assert!(where_is.is_some());
    assert!(
        nav.files.iter().any(|(path, _)| path == "main.rs"),
        "the joined navigator must contain the real source file"
    );
    assert!(nav.usage.is_some() && nav.graph.is_some());
}

/// bug/steering-regressions iteration #4 (live wedge #3, 2026-07-27): the
/// first agentic turn block_on-joined `index_files` over the gathered
/// corpus through the in-process CPU embedder — 40–80 minutes at ~6.5
/// cores, the session frozen between a tool result and the next dispatch.
/// Spawning must return promptly, leave the build running, and adopt only
/// once finished.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_indexing_never_blocks_the_turn() {
    struct GatedEmbedder(std::sync::Arc<tokio::sync::Notify>);
    #[async_trait::async_trait]
    impl newt_core::Embedder for GatedEmbedder {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            // Hold the "model forward" open until the test releases it.
            self.0.notified().await;
            Ok(vec![1.0, 0.0])
        }
    }
    let rt = tokio::runtime::Handle::current();
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let embedder: std::sync::Arc<dyn newt_core::Embedder> =
        std::sync::Arc::new(GatedEmbedder(std::sync::Arc::clone(&release)));
    let index = std::sync::Arc::new(newt_core::SessionSemanticIndex::default());
    let files = vec![("main.rs".to_string(), "pub fn f() {}".to_string())];

    let mut warmup = Some(spawn_semantic_indexing(
        &rt,
        files,
        embedder,
        std::sync::Arc::clone(&index),
        newt_core::OnEmbedFailure::default(),
    ));
    // No stopwatch: the embedder is gated on a Notify nothing has released
    // yet, so a spawn that blocked on it would never return, and reaching
    // this line is the proof that spawning never blocks the turn.
    assert!(
        poll_semantic_indexing(&rt, &mut warmup).is_none(),
        "an unfinished embed is never joined"
    );
    assert!(warmup.is_some(), "the running embed is left running");
    {
        use newt_core::SemanticIndex as _;
        assert_eq!(index.chunks_indexed(), 0, "nothing adopted early");
    }

    // Release the gated forward; the build finishes and a later poll
    // adopts it. `notify_one` stores a permit if the embed has not reached
    // its `notified()` yet, so one call suffices (the former loop re-notified
    // every 25 ms because `notify_waiters` wakes only parked tasks); the
    // build finishing is the event, bounded only by the hang guard.
    release.notify_one();
    let deadline = std::time::Instant::now() + newt_core::test_guard::HANG_GUARD;
    while !warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the released embed never finished"
        );
        tokio::task::yield_now().await;
    }
    let adopted = poll_semantic_indexing(&rt, &mut warmup);
    assert!(warmup.is_none(), "a finished embed is consumed");
    assert!(
        adopted.is_some_and(|n| n >= 1),
        "the finished embed reports its chunk count, got {adopted:?}"
    );
}

/// bug/steering-regressions iteration #3 (live wedges 2026-07-27): the
/// turn must NEVER block on a still-running index warm-up. Two live
/// sessions sat 40+ minutes at ~6 cores — no output, no inference —
/// because the consumer `block_on`-joined an unbounded build. A
/// still-running warm-up stays running; adoption happens on a later turn.
#[tokio::test(flavor = "multi_thread")]
async fn unfinished_warmup_is_left_running_and_the_turn_degrades() {
    let rt = tokio::runtime::Handle::current();
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let job = BackgroundJob::start("indexing repository");
    let completion = job.completion_guard();
    let handle = rt.spawn_blocking(move || {
        let _completion = completion;
        // Hold the "build" open until the test releases it.
        let _ = gate.recv();
        (None, newt_core::NavigatorSession::default())
    });
    let mut warmup = Some(NavWarmup { handle, job });
    let mut where_is = None;
    let mut nav = newt_core::NavigatorSession::default();

    // No stopwatch: a `finish` that joined the gated build would never return
    // (the gate is only released below), so returning at all is the proof.
    finish_nav_warmup(&rt, &mut warmup, &mut where_is, &mut nav);
    assert!(
        warmup.is_some(),
        "a still-running warm-up must be left running for a later turn"
    );
    assert!(
        where_is.is_none(),
        "nothing adopted from an unfinished build"
    );

    // Release the build; a later turn adopts it once it has finished (the
    // event), bounded only by the hang guard.
    release.send(()).unwrap();
    let deadline = std::time::Instant::now() + newt_core::test_guard::HANG_GUARD;
    while !warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
        assert!(
            std::time::Instant::now() < deadline,
            "the released build never finished"
        );
        tokio::task::yield_now().await;
    }
    finish_nav_warmup(&rt, &mut warmup, &mut where_is, &mut nav);
    assert!(warmup.is_none(), "the finished build is adopted next turn");
}

/// #3027: a second session start serves the project model from the persisted
/// drift-cache instead of rescanning it. The stored value is tagged through
/// the public store API; only a warm-up that READ the cache can return the
/// tag (an unchanged unit is never re-derived). Pre-#3027 the warm-up called
/// the uncached `scan_project`, so it never saw the tag.
#[test]
fn second_warmup_serves_the_project_model_from_the_cache() {
    use newt_core::drift_cache;
    let _env = newt_core::process_env::lock();
    let config = tempfile::TempDir::new().unwrap();
    let prev = std::env::var_os("NEWT_CONFIG_DIR");
    std::env::set_var("NEWT_CONFIG_DIR", config.path());
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("Cargo.toml"),
        "[package]\nname = \"warm\"\n",
    )
    .unwrap();
    let ws = workspace.path().to_string_lossy().into_owned();
    let warm = || {
        let mut nav = newt_core::NavigatorSession::default();
        ensure_nav_indexes(
            &ws,
            &newt_core::Config::default(),
            &mut None,
            &mut nav,
            &newt_core::IndexStatus::default(),
            true,
        );
        nav.project.unwrap().units[0].name.clone()
    };

    assert_eq!(warm(), "warm", "cold start derives from the marker");
    let dir = drift_cache::store_dir(config.path(), workspace.path());
    let (name, version) = ("project-model.rust", "project-model.v1");
    let mut cache = drift_cache::load::<newt_core::project_model::ProjectUnit>(&dir, name, version)
        .expect("the cold start persisted the model");
    for entry in cache.entries.values_mut() {
        entry.value.name = "served-from-cache".into();
    }
    drift_cache::save(&dir, name, &cache);
    let second = warm();

    match prev {
        Some(v) => std::env::set_var("NEWT_CONFIG_DIR", v),
        None => std::env::remove_var("NEWT_CONFIG_DIR"),
    }
    assert_eq!(second, "served-from-cache");
}

/// #3026: `[context] index = "off"` skips the navigator warm-up entirely —
/// no background walk is spawned, and the inline fallback builds nothing, so
/// a huge workspace is never walked and the navigator stays on its floor.
#[tokio::test(flavor = "multi_thread")]
async fn index_off_spawns_no_warmup_and_builds_nothing_3026() {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(workspace.path().join("main.rs"), "pub fn m() {}\n").unwrap();
    let workspace = workspace.path().to_string_lossy().into_owned();
    let cfg = newt_core::Config {
        context: Some(newt_core::ContextConfig {
            index: newt_core::config::ContextIndex::Off,
            ..Default::default()
        }),
        ..Default::default()
    };
    let rt = tokio::runtime::Handle::current();
    // The session resolves the switch once and passes it down; the warm-up
    // never reads the process environment.
    let enabled =
        newt_core::config::ContextIndex::resolve(None, &cfg) == newt_core::config::ContextIndex::On;
    assert!(!enabled);
    assert!(
        spawn_nav_warmup(
            &rt,
            &workspace,
            &cfg,
            &newt_core::IndexStatus::default(),
            enabled
        )
        .is_none(),
        "index off must not spawn the warm-up walk"
    );
    let mut where_is = None;
    let mut nav = newt_core::NavigatorSession::default();
    ensure_nav_indexes(
        &workspace,
        &cfg,
        &mut where_is,
        &mut nav,
        &newt_core::IndexStatus::default(),
        enabled,
    );
    assert!(where_is.is_none() && nav.files.is_empty() && nav.usage.is_none());
}
