use super::*;
use newt_core::agentic::venv_cmd_prefix;

/// Run `f` with `set` exported and `clear` removed, restoring every touched
/// variable afterwards. Takes the shared env *write* guard so the
/// env-reading tests elsewhere in this binary (caveat policy / confined
/// shell) never observe a half-mutated environment.
pub(crate) fn with_env_vars<R>(set: &[(&str, &str)], clear: &[&str], f: impl FnOnce() -> R) -> R {
    let _g = crate::test_env_guard::env_write_guard();
    let touched: Vec<String> = set
        .iter()
        .map(|(k, _)| k.to_string())
        .chain(clear.iter().map(|k| k.to_string()))
        .collect();
    let saved: Vec<(String, Option<String>)> = touched
        .iter()
        .map(|k| (k.clone(), std::env::var(k).ok()))
        .collect();
    for k in clear {
        std::env::remove_var(k);
    }
    for (k, v) in set {
        std::env::set_var(k, v);
    }
    let out = f();
    for (k, v) in saved {
        match v {
            Some(val) => std::env::set_var(&k, val),
            None => std::env::remove_var(&k),
        }
    }
    out
}

#[test]
fn cli_fs_grants_widen_read_and_write_scopes() {
    use newt_core::caveats::Scope;
    // Join with the platform path-list separator (`;` on Windows, `:` on
    // Unix), matching how the CLI now writes these vars via join_paths.
    let read_paths = std::env::join_paths(["/home/u/.newt", "/home/u/.hotseat/config.yml"])
        .unwrap()
        .to_string_lossy()
        .into_owned();
    with_env_vars(
        &[
            ("NEWT_READ_PATHS", read_paths.as_str()),
            ("NEWT_WRITE_PATHS", "/home/u/scratch"),
        ],
        &[],
        || {
            // workspace_dev fences BOTH fs_read and fs_write to the workspace
            // (the operator's real case), so both --read and --write widen.
            let tui = newt_core::TuiConfig {
                permissions: newt_core::ToolPermissions {
                    preset: newt_core::PermissionPreset::WorkspaceDev,
                    ..Default::default()
                },
                ..Default::default()
            };
            let cav = policy_for(Some(tui), "/ws");
            let reads = match &cav.fs_read {
                Scope::Only(s) => s,
                Scope::All => panic!("expected scoped fs_read"),
            };
            let writes = match &cav.fs_write {
                Scope::Only(s) => s,
                Scope::All => panic!("expected scoped fs_write"),
            };
            // The workspace fence survives, and the --read grants joined fs_read.
            assert!(reads.contains("/ws"), "workspace still fenced in");
            assert!(reads.contains("/home/u/.newt"));
            assert!(reads.contains("/home/u/.hotseat/config.yml"));
            // --write joins fs_write AND fs_read (write implies read).
            assert!(writes.contains("/home/u/scratch"));
            assert!(reads.contains("/home/u/scratch"), "write implies read");
            // A --write-only path is NOT writable-only by accident: scratch is
            // the sole extra write root; the .newt read grant is not writable.
            assert!(
                !writes.contains("/home/u/.newt"),
                "read grant is not writable"
            );
            // The sandbox is exactly {ws + the 3 grants} — nothing else leaks in.
            assert_eq!(reads.len(), 4, "read sandbox is exactly ws + the grants");
        },
    );
}

#[test]
fn reads_are_locked_to_the_workspace_by_default() {
    use newt_core::caveats::Scope;
    // No grants at all: a fenced preset's reads still flip from All → just the
    // workspace (the operator wants the agent locked to the CWD by default).
    with_env_vars(&[], &["NEWT_READ_PATHS", "NEWT_WRITE_PATHS"], || {
        let tui = newt_core::TuiConfig {
            permissions: newt_core::ToolPermissions {
                preset: newt_core::PermissionPreset::WorkspaceDev,
                ..Default::default()
            },
            ..Default::default()
        };
        match policy_for(Some(tui), "/ws").fs_read {
            Scope::Only(set) => {
                assert_eq!(set.len(), 1, "exactly the workspace");
                assert!(set.contains("/ws"));
            }
            Scope::All => panic!("reads should be locked to the workspace, not All"),
        }
    });
}

#[test]
fn full_access_opts_out_of_the_read_lock() {
    use newt_core::caveats::Scope;
    with_env_vars(&[], &["NEWT_READ_PATHS", "NEWT_WRITE_PATHS"], || {
        let tui = newt_core::TuiConfig {
            permissions: newt_core::ToolPermissions {
                preset: newt_core::PermissionPreset::FullAccess,
                ..Default::default()
            },
            ..Default::default()
        };
        // full_access keeps unrestricted reads — the explicit "no fence" choice.
        assert!(matches!(policy_for(Some(tui), "/ws").fs_read, Scope::All));
    });
}

#[test]
fn read_only_reads_are_confined_to_the_workspace() {
    use newt_core::caveats::{CaveatsExt, Scope};
    // Conscious decision (review of #502): `read_only` ships `fs_read = All`
    // but is the DEFAULT preset, so the CWD-lock confines its reads — an
    // unconfigured session must not read outside the workspace. Unlike
    // full_access (whose writes are also unbounded), it opts out of NOTHING.
    with_env_vars(&[], &["NEWT_READ_PATHS", "NEWT_WRITE_PATHS"], || {
        let tui = newt_core::TuiConfig {
            permissions: newt_core::ToolPermissions {
                preset: newt_core::PermissionPreset::ReadOnly,
                ..Default::default()
            },
            ..Default::default()
        };
        let caveats = policy_for(Some(tui), "/ws");
        assert!(
            matches!(caveats.fs_read, Scope::Only(_)),
            "read_only's broad reads are fenced to the workspace by the lock"
        );
        assert!(caveats.permits_fs_read("/ws"), "the workspace is readable");
        assert!(
            !caveats.permits_fs_read("/etc/passwd"),
            "reads outside the workspace are denied"
        );
    });
}

const DGX_VARS: &[&str] = &[
    "NEWT_DGX_OLLAMA_URL",
    "NEWT_DGX_HOST",
    "NEWT_DGX_SCHEME",
    "NEWT_DGX_OLLAMA_PORT",
    "NEWT_DGX_MODEL",
];

#[test]
fn resolve_backend_config_env_url_wins() {
    with_env_vars(
        &[
            ("NEWT_DGX_OLLAMA_URL", "http://envhost:1234"),
            ("NEWT_DGX_MODEL", "env-model:7b"),
        ],
        DGX_VARS,
        || {
            let choice = resolve_backend_choice(&newt_core::Config::default());
            assert_eq!(choice.url, "http://envhost:1234");
            assert_eq!(choice.model, "env-model:7b");
            assert_eq!(choice.kind, newt_core::BackendKind::Ollama);
        },
    );
}

/// `wizard::tests::probe_candidates_includes_env_host` (another file —
/// off-limits to this module) sets/removes `NEWT_DGX_HOST` WITHOUT taking
/// our guard. If the variable no longer holds the value this test set by
/// the time the call returns, the run raced with that test and the result
/// is meaningless — skip the assertion instead of flaking.
fn host_still(expected: &str) -> bool {
    std::env::var("NEWT_DGX_HOST").as_deref() == Ok(expected)
}

#[test]
fn resolve_backend_config_synthesizes_from_host_scheme_port() {
    with_env_vars(
        &[
            ("NEWT_DGX_HOST", "dgx1.lab"),
            ("NEWT_DGX_SCHEME", "https"),
            ("NEWT_DGX_OLLAMA_PORT", "8443"),
        ],
        DGX_VARS,
        || {
            let choice = resolve_backend_choice(&newt_core::Config::default());
            if !host_still("dgx1.lab") {
                return; // raced with the wizard env test
            }
            assert_eq!(choice.url, "https://dgx1.lab:8443");
        },
    );
    // Host alone uses http + 11434 defaults.
    with_env_vars(&[("NEWT_DGX_HOST", "dgx2")], DGX_VARS, || {
        let choice = resolve_backend_choice(&newt_core::Config::default());
        if !host_still("dgx2") {
            return; // raced with the wizard env test
        }
        assert_eq!(choice.url, "http://dgx2:11434");
    });
}

#[test]
fn resolve_backend_config_falls_back_to_dgx_config_then_localhost() {
    let cfg = newt_core::Config {
        dgx: Some(newt_core::DgxConfig {
            active_model: Some("cfg-model:8b".into()),
            nodes: vec![newt_core::DgxNode {
                name: "n1".into(),
                ollama: Some("http://cfg-node:11434".into()),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    with_env_vars(&[], DGX_VARS, || {
        if std::env::var("NEWT_DGX_HOST").is_ok() {
            return; // raced with the wizard env test (see host_still)
        }
        // Legacy [dgx] shim (one release, #1126): a config whose ONLY
        // extra backend knowledge is the [dgx] block... but note the
        // default Config carries the localhost fallback backend, which as
        // a SOLE backend now outranks the [dgx] shim — the [dgx] path is
        // reached when several backends exist without a default. Pin the
        // shim directly: no backends at all.
        let bare = newt_core::Config {
            backends: vec![],
            ..cfg.clone()
        };
        let choice = resolve_backend_choice(&bare);
        assert_eq!(choice.url, "http://cfg-node:11434");
        assert_eq!(choice.model, "cfg-model:8b");
        // No env, no config → documented localhost defaults.
        let bare2 = newt_core::Config {
            backends: vec![],
            dgx: None,
            ..Default::default()
        };
        let choice = resolve_backend_choice(&bare2);
        assert_eq!(choice.url, "http://localhost:11434");
        assert_eq!(choice.model, "llama3.1:8b");
        assert_eq!(choice.kind, newt_core::BackendKind::Ollama);
        assert!(choice.api_key.is_none());
    });
}

/// Loadout provider/model axis (Slice 2). `NEWT_PROVIDER` selects a named
/// `[backends]` entry by name — regardless of wire protocol — over the
/// historical "prefer the first OpenAI backend" default.
fn backend(
    name: &str,
    endpoint: &str,
    model: &str,
    kind: newt_core::BackendKind,
) -> newt_core::BackendConfig {
    newt_core::BackendConfig {
        name: name.into(),
        endpoint: endpoint.into(),
        model: Some(model.into()),
        model_path: None,
        tiers: vec![],
        kind: Some(kind),
        api: Default::default(),
        api_key_file: None,
        api_key_env: None,
        ..Default::default()
    }
}

#[test]
fn resolve_backend_choice_honors_named_provider() {
    let cfg = newt_core::Config {
        backends: vec![
            // An OpenAI backend that the historical default would pick first…
            backend(
                "remote",
                "http://remote:8000",
                "qwen3:32b",
                newt_core::BackendKind::Openai,
            ),
            // …but NEWT_PROVIDER pins this Ollama one instead.
            backend(
                "local-box",
                "http://local-box:11434",
                "nemotron-3:33b",
                newt_core::BackendKind::Ollama,
            ),
        ],
        ..Default::default()
    };
    with_env_vars(
        &[("NEWT_PROVIDER", "local-box")],
        &["NEWT_DGX_MODEL", "NEWT_BACKEND"],
        || {
            let choice = resolve_backend_choice(&cfg);
            assert_eq!(choice.url, "http://local-box:11434");
            assert_eq!(choice.model, "nemotron-3:33b");
            assert_eq!(choice.kind, newt_core::BackendKind::Ollama);
        },
    );
}

#[test]
fn resolve_backend_choice_named_provider_model_override() {
    let cfg = newt_core::Config {
        backends: vec![backend(
            "dgx-prod",
            "http://dgx:11434",
            "nemotron-3:33b",
            newt_core::BackendKind::Ollama,
        )],
        ..Default::default()
    };
    // The loadout's `model` (→ NEWT_DGX_MODEL) overrides the backend default.
    with_env_vars(
        &[
            ("NEWT_PROVIDER", "dgx-prod"),
            ("NEWT_DGX_MODEL", "nemotron-3:4b"),
        ],
        &["NEWT_BACKEND"],
        || {
            let choice = resolve_backend_choice(&cfg);
            assert_eq!(choice.url, "http://dgx:11434");
            assert_eq!(
                choice.model, "nemotron-3:4b",
                "loadout model overrides backend default"
            );
        },
    );
}

#[test]
fn model_override_applies_on_every_backend_path() {
    // Regression for the `/model <name>` bug: the session model override
    // (NEWT_DGX_MODEL) must win on the pinned-provider path AND the OpenAI
    // default path — previously only the pinned path honored it, so `/model`
    // silently did nothing when a named/OpenAI backend was active.
    let cfg = newt_core::Config {
        backends: vec![
            backend(
                "dgx1",
                "http://dgx1:11434",
                "qwen3:30b",
                newt_core::BackendKind::Ollama,
            ),
            backend(
                "oai",
                "https://api.openai.com/v1",
                "gpt-4.1",
                newt_core::BackendKind::Openai,
            ),
        ],
        ..Default::default()
    };
    // Pinned named backend + override → override wins over the static model.
    with_env_vars(
        &[
            ("NEWT_PROVIDER", "dgx1"),
            ("NEWT_DGX_MODEL", "nemotron:30b"),
        ],
        &["NEWT_BACKEND"],
        || assert_eq!(resolve_backend_choice(&cfg).model, "nemotron:30b"),
    );
    // OpenAI default (no provider pin) + override → override wins too.
    with_env_vars(
        &[("NEWT_DGX_MODEL", "gpt-4.1-mini")],
        &["NEWT_PROVIDER", "NEWT_BACKEND"],
        || {
            let c = resolve_backend_choice(&cfg);
            assert_eq!(c.kind, newt_core::BackendKind::Openai);
            assert_eq!(c.model, "gpt-4.1-mini");
        },
    );
}

#[test]
fn resolve_backend_choice_unknown_provider_falls_through() {
    let cfg = newt_core::Config {
        backends: vec![backend(
            "remote",
            "http://remote:8000",
            "qwen3:32b",
            newt_core::BackendKind::Openai,
        )],
        ..Default::default()
    };
    // A directly-set provider that names no backend is not a hard error here
    // (the loadout path validates upstream) — it falls through to prefer-openai.
    with_env_vars(
        &[("NEWT_PROVIDER", "ghost")],
        &["NEWT_DGX_MODEL", "NEWT_BACKEND"],
        || {
            let choice = resolve_backend_choice(&cfg);
            assert_eq!(choice.url, "http://remote:8000");
            assert_eq!(choice.kind, newt_core::BackendKind::Openai);
        },
    );
}

#[test]
fn backends_list_items_render_label_and_flag_the_active_one() {
    let cfg = newt_core::Config {
        backends: vec![
            backend(
                "dgx1",
                "http://dgx:11434",
                "qwen3:30b",
                newt_core::BackendKind::Ollama,
            ),
            backend(
                "openai",
                "https://api.openai.com/v1",
                "gpt-4.1",
                newt_core::BackendKind::Openai,
            ),
        ],
        ..Default::default()
    };
    let items = backends_list_items(&cfg, Some("openai"));
    assert_eq!(items.len(), 2);
    // Label is the bare, default-colored text (no sigils baked in — the
    // renderer adds the ▸/◀ active styling).
    assert_eq!(items[0].0, "dgx1 · ollama · qwen3:30b @ http://dgx:11434");
    assert!(!items[0].1, "dgx1 is not active");
    assert_eq!(
        items[1].0,
        "openai · openai · gpt-4.1 @ https://api.openai.com/v1"
    );
    assert!(items[1].1, "openai is the active backend");
    // No active name → nothing flagged.
    assert!(backends_list_items(&cfg, None).iter().all(|(_, a)| !a));
}

#[test]
fn active_backend_name_prefers_provider_pin_then_endpoint_match() {
    let cfg = newt_core::Config {
        backends: vec![
            backend(
                "dgx1",
                "http://dgx:11434",
                "qwen3:30b",
                newt_core::BackendKind::Ollama,
            ),
            backend(
                "gnuc",
                "http://gnuc:11434",
                "qwen2.5-coder:14b",
                newt_core::BackendKind::Ollama,
            ),
        ],
        ..Default::default()
    };
    // An explicit NEWT_PROVIDER pin wins.
    with_env_vars(
        &[("NEWT_PROVIDER", "gnuc")],
        &["NEWT_DGX_MODEL", "NEWT_BACKEND"],
        || assert_eq!(active_backend_name(&cfg).as_deref(), Some("gnuc")),
    );
    // No pin → match the resolved endpoint back to a configured backend.
    with_env_vars(
        &[("NEWT_PROVIDER", "dgx1")],
        &["NEWT_DGX_MODEL", "NEWT_BACKEND"],
        || assert_eq!(active_backend_name(&cfg).as_deref(), Some("dgx1")),
    );
}

#[test]
fn slash_backends_list_and_switch_keep_session_alive() {
    let cfg = newt_core::Config {
        backends: vec![backend(
            "dgx1",
            "http://dgx:11434",
            "qwen3:30b",
            newt_core::BackendKind::Ollama,
        )],
        ..Default::default()
    };
    let _ = cfg; // dispatch_slash re-resolves real config; this just documents intent.
                 // Listing returns true (session continues) regardless of configured set.
    with_env_vars(&[], &["NEWT_PROVIDER", "NEWT_DGX_MODEL"], || {
        assert!(dispatch_slash("/backends", "/ws", false, false, false).unwrap());
        // An unknown name reports the miss but still keeps the session alive.
        assert!(dispatch_slash("/backends nope-xyz", "/ws", false, false, false).unwrap());
    });
}

#[test]
fn help_lists_backends_command() {
    assert!(help_lines().iter().any(|l| l.contains("/backends")));
}

#[test]
fn slash_crew_usage_and_unknown_keep_session_alive() {
    // Bare `/crew` prints usage; an unknown subcommand reports the miss.
    // (`/crew edit` is exercised by crew_form's own tests — invoking it here
    // would read real stdin and write to ~/.newt, so it's deliberately not
    // dispatched in a unit test.)
    assert!(dispatch_slash("/crew", "/ws", false, false, false).unwrap());
    assert!(dispatch_slash("/crew bogus", "/ws", false, false, false).unwrap());
}

#[test]
fn help_lists_crew_edit_command() {
    assert!(help_lines().iter().any(|l| l.contains("/crew edit")));
}

#[test]
fn venv_cmd_prefix_builds_exports_or_none() {
    let venv_vars: &[&str] = &["NEWT_VENV", "VIRTUAL_ENV", "NEWT_EXEC_PATHS"];
    // Nothing set → no prefix at all.
    with_env_vars(&[], venv_vars, || {
        assert!(venv_cmd_prefix().is_none());
    });
    // NEWT_VENV → VIRTUAL_ENV export + venv/bin on PATH, single-quoted.
    with_env_vars(&[("NEWT_VENV", "/opt/my venv")], venv_vars, || {
        let p = venv_cmd_prefix().unwrap();
        assert!(p.contains("export VIRTUAL_ENV='/opt/my venv'"), "got: {p}");
        assert!(p.contains("'/opt/my venv/bin'"), "venv bin on PATH: {p}");
        assert!(p.contains(":\"$PATH\""), "PATH is prepended, not replaced");
    });
    // VIRTUAL_ENV is the fallback when NEWT_VENV is absent.
    with_env_vars(&[("VIRTUAL_ENV", "/opt/fallback")], venv_vars, || {
        let p = venv_cmd_prefix().unwrap();
        assert!(p.contains("export VIRTUAL_ENV='/opt/fallback'"), "got: {p}");
    });
    // Exec paths alone → PATH export only, no VIRTUAL_ENV.
    with_env_vars(&[("NEWT_EXEC_PATHS", "/a/bin:/b/bin")], venv_vars, || {
        let p = venv_cmd_prefix().unwrap();
        assert!(!p.contains("VIRTUAL_ENV"), "got: {p}");
        assert!(p.contains("'/a/bin':'/b/bin'"), "got: {p}");
    });
    // Embedded single quote is sh-escaped.
    with_env_vars(&[("NEWT_VENV", "/o'dir")], venv_vars, || {
        let p = venv_cmd_prefix().unwrap();
        assert!(p.contains(r"'/o'\''dir'"), "got: {p}");
    });
}

#[cfg(unix)]
#[serial_test::serial(real_fs)]
#[test]
fn scan_cli_exec_grants_collects_only_executables() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::TempDir::new().unwrap();
    let exe = dir.path().join("mytool");
    std::fs::write(&exe, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.path().join("README"), "not executable").unwrap();
    let dir_str = dir.path().to_string_lossy().into_owned();

    with_env_vars(
        &[("NEWT_EXEC_PATHS", &dir_str)],
        &["NEWT_VENV", "VIRTUAL_ENV"],
        || {
            let grants = scan_cli_exec_grants();
            assert!(grants.contains(&"mytool".to_string()), "got: {grants:?}");
            assert!(
                !grants.contains(&"README".to_string()),
                "non-executables excluded: {grants:?}"
            );

            // And policy_for widens a Scope::Only exec set with the grants.
            let tui = newt_core::TuiConfig::default(); // WorkspaceDev preset
            let policy = policy_for(Some(tui), "/ws");
            use newt_core::CaveatsExt;
            assert!(
                policy.permits_exec("mytool"),
                "CLI exec grant must widen the session exec scope"
            );
        },
    );
}

#[test]
fn real_context_discovery_resolves_model_over_tui_default_false() {
    // Default (no config): trust the declared window.
    assert!(!real_context_discovery(&newt_core::Config::default(), "m"));

    // [tui] opts into empirical discovery globally.
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            real_context_discovery: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert!(real_context_discovery(&cfg, "m"));

    // A per-model Some(false) overrides the global true.
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            real_context_discovery: Some(true),
            ..Default::default()
        }),
        model_tuning: vec![newt_core::config::ModelTuning {
            model: "m".into(),
            num_ctx: None,
            context_window: None,
            real_context_discovery: Some(false),
            mid_loop_trim_threshold: None,
            mid_loop_trim_tokens: None,
            max_tool_rounds: None,
            workflow_grace_rounds: None,
            narration_nudge_cap: None,
        }],
        ..Default::default()
    };
    assert!(
        !real_context_discovery(&cfg, "m"),
        "per-model override wins"
    );
    assert!(
        real_context_discovery(&cfg, "other"),
        "a different model still inherits the [tui] default"
    );
}

#[test]
fn num_ctx_env_overrides_config_and_ignores_garbage() {
    let cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            num_ctx: Some(8192),
            ..Default::default()
        }),
        ..Default::default()
    };
    with_env_vars(&[("NEWT_NUM_CTX", "4096")], &[], || {
        assert_eq!(num_ctx(&cfg), Some(4096), "env wins over config");
    });
    with_env_vars(&[("NEWT_NUM_CTX", "not-a-number")], &[], || {
        assert_eq!(num_ctx(&cfg), Some(8192), "garbage env falls back");
    });
    with_env_vars(&[], &["NEWT_NUM_CTX"], || {
        assert_eq!(num_ctx(&cfg), Some(8192));
        assert_eq!(num_ctx(&newt_core::Config::default()), None);
    });
}

#[test]
fn verbose_mode_reads_chat_style_env() {
    with_env_vars(&[("NEWT_CHAT_STYLE", "VERBOSE")], &[], || {
        assert!(verbose_mode());
    });
    with_env_vars(&[("NEWT_CHAT_STYLE", "compact")], &[], || {
        assert!(!verbose_mode());
    });
    with_env_vars(&[], &["NEWT_CHAT_STYLE"], || {
        assert!(!verbose_mode());
    });
}

#[test]
fn debug_mode_env_or_config() {
    let dbg_cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            debug: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    with_env_vars(&[], &["NEWT_DEBUG"], || {
        assert!(debug_mode(&dbg_cfg), "config debug=true is enough");
        assert!(!debug_mode(&newt_core::Config::default()));
    });
    with_env_vars(&[("NEWT_DEBUG", "1")], &[], || {
        assert!(debug_mode(&newt_core::Config::default()), "env wins");
    });
}

#[test]
fn trace_mode_env_or_config_and_implies_debug() {
    let trace_cfg = newt_core::Config {
        tui: Some(newt_core::TuiConfig {
            trace: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    with_env_vars(&[], &["NEWT_TRACE", "NEWT_DEBUG"], || {
        assert!(trace_mode(&trace_cfg), "config trace=true is enough");
        assert!(debug_mode(&trace_cfg), "trace implies debug");
        assert!(!trace_mode(&newt_core::Config::default()));
    });
    with_env_vars(&[("NEWT_TRACE", "1")], &["NEWT_DEBUG"], || {
        assert!(trace_mode(&newt_core::Config::default()), "env wins");
        assert!(
            debug_mode(&newt_core::Config::default()),
            "env trace implies debug"
        );
    });
}

#[test]
fn runtime_context_block_exposes_model_harness_and_backend() {
    let id = newt_core::AgentIdentity::default();
    let block = runtime_context_block(
        "qwen3:30b",
        "http://REDACTED-HOST:11434",
        newt_core::BackendKind::Ollama,
        &id,
    );
    assert!(block.contains("Model: qwen3:30b"), "{block}");
    assert!(block.contains("newt-agent v"), "harness + version: {block}");
    assert!(
        block.contains("ollama @ http://REDACTED-HOST:11434"),
        "{block}"
    );
    assert!(block.contains("Current date/time:"), "{block}");
    // Steers against confabulated identities (the bug this fixes).
    assert!(block.contains("never invent"), "{block}");
    // OpenAI backend is labeled accordingly.
    let oa = runtime_context_block(
        "gpt-4.1",
        "https://api.openai.com",
        newt_core::BackendKind::Openai,
        &id,
    );
    assert!(
        oa.contains("openai-compatible @ https://api.openai.com"),
        "{oa}"
    );
}

#[test]
fn yolo_runtime_authority_note_tracks_disable_ocap_env() {
    with_env_vars(&[], &["NEWT_DISABLE_OCAP"], || {
        assert!(yolo_runtime_authority_note().is_none());
    });

    with_env_vars(&[("NEWT_DISABLE_OCAP", "1")], &[], || {
        let note = yolo_runtime_authority_note().expect("yolo note");
        assert!(note.contains("--disable-ocap/--yolo is active"), "{note}");
        assert!(
            note.contains("run_command uses the unconfined host shell"),
            "{note}"
        );
        assert!(
            note.contains("not the brush/agent-bridle confined shell"),
            "{note}"
        );
        assert!(note.contains("web_fetch remains net-leashed"), "{note}");
        assert!(!note.contains("web_fetch uses the unconfined"), "{note}");
    });
}

#[test]
fn runtime_context_block_includes_yolo_authority_note_only_when_active() {
    let id = newt_core::AgentIdentity::default();
    with_env_vars(&[], &["NEWT_DISABLE_OCAP"], || {
        let block =
            runtime_context_block("qwen3:30b", "http://h", newt_core::BackendKind::Ollama, &id);
        assert!(
            !block.contains("--disable-ocap/--yolo is active"),
            "{block}"
        );
    });

    with_env_vars(&[("NEWT_DISABLE_OCAP", "1")], &[], || {
        let block =
            runtime_context_block("qwen3:30b", "http://h", newt_core::BackendKind::Ollama, &id);
        assert!(block.contains("# Runtime authority"), "{block}");
        assert!(
            block.contains("Do not claim run_command is unavailable due to brush in this mode"),
            "{block}"
        );
        assert!(
            block
                .contains("Native fs tools remain workspace-fenced; web_fetch remains net-leashed"),
            "{block}"
        );
    });
}

#[test]
fn prompt_str_expands_newt_prompt_template() {
    // A user template (NEWT_PROMPT) is used verbatim — the user owns it, so
    // no auto `[i]` prefix; `\M` surfaces the edit mode for those who want
    // it, and the model/rich args don't interfere with an explicit template.
    with_env_vars(&[("NEWT_PROMPT", "\\w \\M \\v> ")], &[], || {
        let vi = prompt_str("/tmp/proj", true, "gpt-4.1", true);
        assert_eq!(vi, format!("proj vi {VERSION}> "));
        let em = prompt_str("/tmp/proj", false, "gpt-4.1", false);
        assert_eq!(em, format!("proj emacs {VERSION}> "));
    });
}
