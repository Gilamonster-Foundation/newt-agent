use super::*;

fn read_grant(path: &std::path::Path) -> Caveats {
    Caveats {
        fs_read: Scope::only([path.to_string_lossy().into_owned()]),
        fs_write: Scope::none(),
        exec: Scope::none(),
        net: Scope::none(),
        ..Caveats::top()
    }
}

async fn search(ws: &std::path::Path, caveats: &Caveats, path: Option<&str>) -> String {
    run_tool_with_disposition(
        "text_search",
        serde_json::json!({"query": "NAV_AUTHORITY_SENTINEL", "path": path}),
        ws,
        caveats,
        &mut NoMcp,
        None,
        None,
        PromptDisposition::Act,
    )
    .await
}

#[test]
fn plan_advertises_registered_navigation_reads_only() {
    let definitions = merged_tool_definitions(
        &NoMcp, false, false, false, None, false, false, false, false, false, false, false, false,
    );
    let defs = filter_tools_for_disposition(definitions, PromptDisposition::Plan);
    for name in crate::navigator::NAV_TOOL_NAMES {
        assert!(crate::agentic::is_read_only_tool(name), "{name}");
        assert!(
            defs.as_array()
                .unwrap()
                .iter()
                .any(|def| def["function"]["name"] == *name),
            "Plan must advertise navigation read {name}"
        );
    }
    for name in [
        "write_file",
        "run_command",
        "request_permissions",
        "web_fetch",
    ] {
        assert!(!tool_allowed(PromptDisposition::Plan, name), "{name}");
    }
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn model_entered_plan_can_search_actual_files_without_widening_authority() {
    #[derive(Default)]
    struct Control(std::sync::atomic::AtomicBool);
    impl crate::agentic::PlanModeControl for Control {
        fn is_plan_mode(&self) -> bool {
            self.0.load(std::sync::atomic::Ordering::Acquire)
        }
        fn set_plan_mode(&self, active: bool) -> Result<(), String> {
            self.0.store(active, std::sync::atomic::Ordering::Release);
            Ok(())
        }
        fn request_exit(&self) -> Result<(), String> {
            Ok(())
        }
        fn take_exit_requested(&self) -> bool {
            false
        }
    }
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("evidence.rs"),
        "fn NAV_AUTHORITY_SENTINEL() {}\n",
    )
    .unwrap();
    let ledger = crate::agentic::scheduled::SessionStepLedger::default();
    let control = Control::default();
    let grant = read_grant(ws.path());
    for (name, args) in [
        ("enter_plan_mode", serde_json::json!({})),
        (
            "text_search",
            serde_json::json!({"query": "NAV_AUTHORITY_SENTINEL"}),
        ),
    ] {
        let out = execute_tool_with_collaborators(
            name,
            &args,
            &ws.path().to_string_lossy(),
            false,
            20,
            &grant,
            &mut NoMcp,
            ToolCollaborators {
                step_ledger: Some(&ledger),
                plan_mode_control: Some(&control),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        if name == "text_search" {
            assert!(
                out.contains("evidence.rs") && out.contains("fn NAV_AUTHORITY_SENTINEL"),
                "{out}"
            );
        } else {
            assert!(out.contains("PLAN MODE"), "{out}");
        }
    }
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn text_search_requires_the_requested_read_scope() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::create_dir(ws.path().join("allowed")).unwrap();
    let allowed = ws.path().join("allowed/evidence.rs");
    std::fs::write(&allowed, "NAV_AUTHORITY_SENTINEL allowed\n").unwrap();
    std::fs::write(
        ws.path().join("private.rs"),
        "NAV_AUTHORITY_SENTINEL private\n",
    )
    .unwrap();
    for (grant, path) in [
        (read_grant(&allowed), Some("allowed/evidence.rs")),
        (read_grant(&ws.path().join("allowed")), Some("allowed")),
    ] {
        let out = search(ws.path(), &grant, path).await;
        assert!(out.contains("NAV_AUTHORITY_SENTINEL allowed"), "{out}");
        assert!(!out.contains("NAV_AUTHORITY_SENTINEL private"), "{out}");
    }
    for (grant, path) in [
        (
            Caveats {
                fs_read: Scope::none(),
                ..read_grant(ws.path())
            },
            Some("allowed/evidence.rs"),
        ),
        (read_grant(&allowed), None),
        (read_grant(&ws.path().join("allowed")), Some("private.rs")),
    ] {
        let out = search(ws.path(), &grant, path).await;
        assert!(out.contains("capability denied: fs_read"), "{out}");
        assert!(
            !out.contains("NAV_AUTHORITY_SENTINEL allowed")
                && !out.contains("NAV_AUTHORITY_SENTINEL private"),
            "{out}"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn text_search_never_follows_file_or_directory_links_outside_workspace() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("workspace");
    let outside = root.path().join("outside");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(
        outside.join("secret.rs"),
        "NAV_AUTHORITY_SENTINEL outside\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.join("secret.rs"), ws.join("file-link.rs")).unwrap();
    std::os::unix::fs::symlink(&outside, ws.join("dir-link")).unwrap();
    for path in [
        None,
        Some("file-link.rs"),
        Some("dir-link"),
        Some("../outside"),
        outside.to_str(),
    ] {
        let out = search(&ws, &read_grant(&ws), path).await;
        assert!(
            !out.contains("NAV_AUTHORITY_SENTINEL outside"),
            "{path:?}: {out}"
        );
        assert!(
            out.contains("denied") || out.contains("warning"),
            "escape must be visible: {out}"
        );
    }
}

#[tokio::test]
async fn cached_navigation_requires_its_whole_workspace_read_grant() {
    let ws = tempfile::tempdir().unwrap();
    let index = crate::where_is::WhereIsIndex::from_facts(
        vec![("SecretSymbol".into(), "private.rs".into(), "fn".into())],
        vec![],
    );
    let files = [("private.rs".into(), "fn SecretSymbol() {}".into())];
    let workspace = ws.path().to_string_lossy();
    for (grant, allowed) in [
        (
            Caveats {
                fs_read: Scope::none(),
                ..read_grant(ws.path())
            },
            false,
        ),
        (read_grant(&ws.path().join("allowed.rs")), false),
        (Caveats::top(), true),
        (
            read_grant(ws.path()),
            cfg!(any(target_os = "linux", target_os = "macos")),
        ),
    ] {
        let out = execute_tool_with_collaborators(
            "goto_definition",
            &serde_json::json!({"symbol": "SecretSymbol"}),
            &workspace,
            false,
            20,
            &grant,
            &mut NoMcp,
            ToolCollaborators {
                nav: Some(crate::navigator::NavToolCtx {
                    workspace: &workspace,
                    where_is: Some(&index),
                    files: Some(&files),
                    usage: None,
                    graph: None,
                    project: None,
                    status: None,
                }),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        if allowed {
            assert!(out.contains("fn SecretSymbol"), "{out}");
        } else {
            assert!(out.contains("capability denied: fs_read"), "{out}");
            assert!(!out.contains("fn SecretSymbol"), "{out}");
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn impact_does_not_read_coverage_through_an_escaping_link() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("workspace");
    std::fs::create_dir(&ws).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = 'local'\nversion = '0.1.0'\n",
    )
    .unwrap();
    let secret = root.path().join("external.info");
    std::fs::write(
        &secret,
        "SF:PRIVATE_COVERAGE_SOURCE\nLH:7\nLF:9\nend_of_record\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&secret, ws.join("lcov.info")).unwrap();
    let model = crate::project_model::ProjectModel::default();
    let workspace = ws.to_string_lossy();
    let out = execute_tool_with_collaborators(
        "impact",
        &serde_json::json!({"unit": "local"}),
        &workspace,
        false,
        20,
        &read_grant(&ws),
        &mut NoMcp,
        ToolCollaborators {
            nav: Some(crate::navigator::NavToolCtx {
                workspace: &workspace,
                project: Some(&model),
                where_is: None,
                files: None,
                usage: None,
                graph: None,
                status: None,
            }),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!out.contains("PRIVATE_COVERAGE_SOURCE"), "{out}");
    assert!(
        out.contains("denied"),
        "coverage refusal must remain visible: {out}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn impact_rebuilds_only_admitted_project_markers_and_coverage() {
    let root = tempfile::tempdir().unwrap();
    let ws = root.path().join("workspace");
    let outside = root.path().join("outside");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(
        ws.join("Cargo.toml"),
        "[workspace]\nmembers = ['../outside']\n",
    )
    .unwrap();
    std::fs::write(
        outside.join("Cargo.toml"),
        "[package]\nname = 'local'\nversion = '0.1.0'\n[dependencies]\nPRIVATE_PROJECT_DEP = '1'\n",
    )
    .unwrap();
    let model =
        crate::project_model::scan_project(&ws, &crate::project_model::builtin_project_packs())
            .unwrap();
    let workspace = ws.to_string_lossy();
    let call = || async {
        execute_tool_with_collaborators(
            "impact",
            &serde_json::json!({"unit": "local"}),
            &workspace,
            false,
            20,
            &read_grant(&ws),
            &mut NoMcp,
            ToolCollaborators {
                nav: Some(crate::navigator::NavToolCtx {
                    workspace: &workspace,
                    project: Some(&model),
                    where_is: None,
                    files: None,
                    usage: None,
                    graph: None,
                    status: None,
                }),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
    };
    let out = call().await.unwrap().unwrap();
    assert!(out.contains("capability denied: fs_read"), "{out}");
    assert!(!out.contains("PRIVATE_PROJECT_DEP"), "{out}");
    std::fs::write(
        ws.join("Cargo.toml"),
        "[package]\nname = 'local'\nversion = '0.1.0'\n[dependencies]\npublic_dependency = '1'\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("lcov.info"),
        "SF:local.rs\nLH:7\nLF:9\nend_of_record\n",
    )
    .unwrap();
    let out = call().await.unwrap().unwrap();
    assert!(
        out.contains("public_dependency") && out.contains("local.rs  hit=7 found=9"),
        "{out}"
    );
    assert!(
        !out.contains("PRIVATE_PROJECT_DEP"),
        "stale unbounded model must not be reused: {out}"
    );
}

#[tokio::test]
async fn text_search_with_no_read_grant_never_reads_on_any_platform() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(
        ws.path().join("private.rs"),
        "NAV_AUTHORITY_SENTINEL private\n",
    )
    .unwrap();
    let denied = Caveats {
        fs_read: Scope::none(),
        ..read_grant(ws.path())
    };
    let out = search(ws.path(), &denied, None).await;
    assert!(out.contains("capability denied: fs_read"), "{out}");
    assert!(!out.contains("NAV_AUTHORITY_SENTINEL private"), "{out}");
}

#[test]
fn unrestricted_navigation_reads_remain_available_on_every_platform() {
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("evidence.rs"), "NAV_AUTHORITY_SENTINEL\n").unwrap();
    std::fs::write(
        ws.path().join("Cargo.toml"),
        "[package]\nname = 'local'\nversion = '0.1.0'\n",
    )
    .unwrap();
    std::fs::write(
        ws.path().join("lcov.info"),
        "SF:local.rs\nLH:7\nLF:9\nend_of_record\n",
    )
    .unwrap();
    let workspace = ws.path().to_string_lossy();
    let model = crate::project_model::scan_project(
        ws.path(),
        &crate::project_model::builtin_project_packs(),
    )
    .unwrap();
    let ctx = crate::navigator::NavToolCtx {
        workspace: &workspace,
        project: Some(&model),
        files: None,
        where_is: None,
        usage: None,
        graph: None,
        status: None,
    };
    for (name, args, expected) in [
        (
            "text_search",
            serde_json::json!({"query": "NAV_AUTHORITY_SENTINEL"}),
            "evidence.rs",
        ),
        (
            "impact",
            serde_json::json!({"unit": "local"}),
            "local.rs  hit=7 found=9",
        ),
    ] {
        let out = crate::agentic::tools::navigation::execute(
            name,
            &args,
            &workspace,
            &Caveats::top(),
            &ctx,
        );
        assert!(out.contains(expected), "{name}: {out}");
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let scoped = crate::agentic::tools::navigation::execute(
                name,
                &args,
                &workspace,
                &read_grant(ws.path()),
                &ctx,
            );
            assert!(scoped.contains("unavailable"), "{name}: {scoped}");
            assert!(!scoped.contains(expected), "{name}: {scoped}");
        }
    }
}
