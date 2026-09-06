/// The two guard families are ONE lock — asserted on the CALLING thread,
/// which is what makes it deterministic.
///
/// A cross-thread `try_lock` probe was the obvious formulation and is the
/// wrong one: a failed probe only says *somebody* holds the lock, and a
/// sibling test satisfies that just as well. Verified the hard way — with
/// the locks deliberately re-split, the probe version still PASSED in a
/// full run (a sibling was holding it) and only failed when run alone. A
/// regression test for a flake must not itself depend on scheduling, so
/// this asks the thread-local question instead.
///
/// Non-vacuous: re-split `GlobalSettingsGuard` onto its own mutex and this
/// fails immediately, in isolation or in a full run.
#[test]
fn the_settings_guard_and_the_env_guard_are_one_lock() {
    assert!(
        !newt_core::process_env::held_by_current_thread(),
        "a fresh test thread holds no environment"
    );
    let settings = newt_core::test_guard::GlobalSettingsGuard::acquire();
    assert!(
        newt_core::process_env::held_by_current_thread(),
        "GlobalSettingsGuard must hold the process-env lock — otherwise it \
         is a second lock over the same variables, which is #1850"
    );
    drop(settings);
    assert!(!newt_core::process_env::held_by_current_thread());

    let env = crate::test_env_guard::env_write_guard();
    assert!(
        newt_core::process_env::held_by_current_thread(),
        "test_env_guard must hold the SAME lock — this is the other half \
         of the pair that raced"
    );
    drop(env);
    assert!(!newt_core::process_env::held_by_current_thread());
}

/// …and that one lock really does exclude other threads, so the identity
/// above is worth something. Deterministic: we hold it, so nothing else
/// can, whatever the scheduler does with the rest of the suite.
#[test]
fn the_env_guard_excludes_every_other_thread() {
    let env = crate::test_env_guard::env_write_guard();
    let taken = std::thread::spawn(|| newt_core::process_env::try_lock().is_some())
        .join()
        .expect("probe thread");
    assert!(
        !taken,
        "a second thread entered the process environment while the env \
         write guard was held"
    );
    drop(env);
}

/// A guarded test may drive production code that writes the environment —
/// `apply_backend_choice_refuses_embedded_before_any_mutation` and
/// `browsing_backends_marks_no_preference_action` both do. That is why the
/// lock is reentrant, and this is the assertion that says so: a plain
/// mutex hangs here instead of failing, so a regression shows up as a
/// wedged suite rather than a red test — worth pinning explicitly.
#[test]
fn a_held_guard_does_not_block_the_production_writer_on_its_own_thread() {
    let _env = crate::test_env_guard::env_write_guard();
    let saved = std::env::var("NEWT_OPENAI_API").ok();
    crate::apply_openai_api_env(newt_core::OpenAiApi::Responses);
    assert_eq!(
        std::env::var("NEWT_OPENAI_API").ok().as_deref(),
        Some("responses"),
        "the production writer must complete while this thread holds the lock"
    );
    newt_core::process_env::set_or_remove("NEWT_OPENAI_API", saved.as_deref());
}

/// Production env writes go through `newt_core::process_env`, never raw
/// `env::set_var`. A TRIPWIRE in this repo's ratchet idiom: per-file
/// occurrence counts that may only go DOWN. `commands/model.rs` and
/// `commands/settings.rs` sit at zero — they hold no test-side env writes
/// at all — so any reappearance there is a new unguarded production
/// writer. `lib.rs`'s figure is its `#[cfg(test)]` modules, which mutate
/// the environment legitimately while holding a guard.
///
/// The needles are assembled with `concat!` so this test's own source,
/// which `include_str!` pulls in below, cannot match them. The sources are
/// embedded at COMPILE time, so the unit tier stays filesystem-free.
#[test]
fn production_env_writes_go_through_the_process_env_lock() {
    const SET: &str = concat!("std::env::", "set_var(");
    const REMOVE: &str = concat!("std::env::", "remove_var(");
    for (name, src, baseline) in [
        (
            "commands/model.rs",
            include_str!("../commands/model.rs"),
            0usize,
        ),
        (
            "commands/settings.rs",
            include_str!("../commands/settings.rs"),
            0,
        ),
        ("lib.rs", include_str!("../lib.rs"), 15),
        // This module moved out of `lib.rs` (it was an inline
        // `#[cfg(test)] mod`). Without its own row the relocated
        // env-isolation tests would be scanned by nothing.
        (
            "lib_tests/process_env_isolation_tests.rs",
            include_str!("process_env_isolation_tests.rs"),
            0,
        ),
    ] {
        let found = src.matches(SET).count() + src.matches(REMOVE).count();
        assert!(
            found <= baseline,
            "{name}: {found} direct env mutations, baseline {baseline} — a new \
             one must go through newt_core::process_env (#1850). This baseline \
             ratchets DOWN only; lower it in the PR that removes a site."
        );
    }
}
