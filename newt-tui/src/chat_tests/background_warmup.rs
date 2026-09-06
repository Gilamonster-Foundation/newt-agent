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
    let mut warmup = Some(spawn_nav_warmup(
        &rt,
        &workspace,
        &newt_core::Config::default(),
        &newt_core::IndexStatus::default(),
    ));
    let job = warmup.as_ref().unwrap().job.clone();
    let mut where_is = None;
    let mut nav = newt_core::NavigatorSession::default();

    // Iteration #3 contract: adoption happens only once the build is done —
    // wait for readiness (bounded), then adopt. A still-running warm-up is
    // covered by `unfinished_warmup_is_left_running_and_the_turn_degrades`.
    for _ in 0..200 {
        if warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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

    let started = std::time::Instant::now();
    let mut warmup = Some(spawn_semantic_indexing(
        &rt,
        files,
        embedder,
        std::sync::Arc::clone(&index),
        newt_core::OnEmbedFailure::default(),
    ));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "spawning the embed must never block the turn"
    );
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
    // adopts it.
    release.notify_waiters();
    for _ in 0..200 {
        if warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
            break;
        }
        release.notify_waiters();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
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

    let started = std::time::Instant::now();
    finish_nav_warmup(&rt, &mut warmup, &mut where_is, &mut nav);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "finish must return promptly, never join an unfinished build"
    );
    assert!(
        warmup.is_some(),
        "a still-running warm-up must be left running for a later turn"
    );
    assert!(
        where_is.is_none(),
        "nothing adopted from an unfinished build"
    );

    // Release the build; a later turn adopts it.
    release.send(()).unwrap();
    for _ in 0..200 {
        if warmup.as_ref().is_some_and(|w| w.handle.is_finished()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    finish_nav_warmup(&rt, &mut warmup, &mut where_is, &mut nav);
    assert!(warmup.is_none(), "the finished build is adopted next turn");
}
