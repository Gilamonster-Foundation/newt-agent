//! Process-level coverage for `newt mcp add|remove|list|install|import`.
//!
//! The config root is redirected via `NEWT_CONFIG_DIR`, and `HOME` + the
//! working directory point at tempdirs so the merged `list` view never reads
//! the developer's real `~/.claude.json` / `./.mcp.json` (the doctor_cli.rs
//! isolation pattern).
//!
//! `mcp import` cases execute the real CLI and filesystem. They ground the pure
//! parser, sanitizer, selection, and staged-write regressions in
//! `newt-core::mcp`, `newt-core::config`, and `newt-cli::mcp_cmd`; each is
//! ignored in per-PR CI and run serially by the weekly/release acceptance lane.
//!
//! Model: GPT-5 | Harness: Codex | Operator: Shawn Hartsock | Time: 15:22 EDT | Date: 2026-08-12

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;

struct Sandbox {
    _root: tempfile::TempDir,
    config_dir: std::path::PathBuf,
    home: std::path::PathBuf,
    cwd: std::path::PathBuf,
}

fn sandbox() -> Sandbox {
    let root = tempfile::tempdir().unwrap();
    let config_dir = root.path().join("cfg");
    let home = root.path().join("home");
    // The workspace MUST live UNDER the sandbox home (#1494). The project-config
    // walk-up (`find_project_config_from`) stops at `home_dir()`; if `cwd` and
    // `home` were siblings, the boundary is never an ancestor of `cwd`, so on
    // Windows — where the temp dir lives under `C:\Users\<user>` — the walk-up
    // sails up past the real home and writes fixtures into the developer's real
    // `~/.newt/config.toml`. Nesting `cwd` under `home` makes the boundary a true
    // ancestor, so the search is contained on every OS (production is already
    // safe: there the real home genuinely is an ancestor of `cwd`).
    let cwd = home.join("ws");
    for dir in [&config_dir, &home, &cwd] {
        std::fs::create_dir_all(dir).unwrap();
    }
    Sandbox {
        _root: root,
        config_dir,
        home,
        cwd,
    }
}

/// A `newt` invocation isolated from the developer's environment.
fn newt(sb: &Sandbox) -> Command {
    let mut cmd = Command::cargo_bin("newt").unwrap();
    cmd.env("NEWT_CONFIG_DIR", &sb.config_dir)
        .env("HOME", &sb.home)
        // `home_dir()` reads HOME then USERPROFILE; set both so home resolution is
        // contained on Windows too, not just Unix (#1494).
        .env("USERPROFILE", &sb.home)
        .env_remove("NEWT_CONFIG")
        .current_dir(&sb.cwd);
    cmd
}

fn load_config(path: &Path) -> newt_core::Config {
    newt_core::Config::load(path).unwrap()
}

// Families under `mcp_cli/`. The composed private-MCP UAT stays inline
// below: it is one test in an already-namespaced module with its own
// imports, and un-nesting it would cost a dedent for no gain.
#[path = "mcp_cli/config_targets.rs"]
mod config_targets;
#[path = "mcp_cli/crud.rs"]
mod crud;
#[path = "mcp_cli/grant_net.rs"]
mod grant_net;
#[path = "mcp_cli/import_dedup.rs"]
mod import_dedup;
#[path = "mcp_cli/import_secret_rejection.rs"]
mod import_secret_rejection;
#[path = "mcp_cli/import_targets.rs"]
mod import_targets;
#[path = "mcp_cli/install_catalog.rs"]
mod install_catalog;

// ---------------------------------------------------------------------------
// Composed import -> discovery -> live MCP -> private-URL recovery UAT
// ---------------------------------------------------------------------------

mod composed_private_mcp_uat {
    use super::*;
    use std::ffi::OsString;

    use newt_core::agentic::{LeasedMcpCall, McpTools, PromptIntake};
    use newt_core::{BackendKind, ChatCtx, CompactionTriggerPolicy, MemMessage, ToolEvent};
    use newt_mcp_client::McpToolset;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    const REVIEW_TOOL: &str = "review_source__get_review";
    const MCP_RESULT: &str = "authenticated review 42 loaded from imported MCP";

    /// Restore process-global discovery inputs after the acceptance scenario.
    /// The test is ignored and serialized because it intentionally grounds the
    /// environment and real-filesystem seams used by production discovery.
    struct DiscoveryEnv {
        saved: Vec<(&'static str, Option<OsString>)>,
    }

    impl DiscoveryEnv {
        fn install(sb: &Sandbox) -> Self {
            let values = [
                ("NEWT_CONFIG_DIR", sb.config_dir.as_os_str()),
                ("HOME", sb.home.as_os_str()),
                ("USERPROFILE", sb.home.as_os_str()),
            ];
            let saved = values
                .iter()
                .map(|(key, value)| {
                    let previous = std::env::var_os(key);
                    std::env::set_var(key, value);
                    (*key, previous)
                })
                .collect();
            Self { saved }
        }
    }

    impl Drop for DiscoveryEnv {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..).rev() {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    fn last_tool_result(body: &serde_json::Value) -> Option<&str> {
        body["messages"]
            .as_array()?
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")?
            .get("content")?
            .as_str()
    }

    fn ollama_tool_call(name: &str, arguments: serde_json::Value) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": name,
                        "arguments": arguments
                    }
                }]
            }
        }))
    }

    /// Adaptive simulated inference. Any missing recovery seam deliberately
    /// reproduces the field-observed shell fallback so the final assertions
    /// distinguish a genuinely composed route from a merely connected server.
    struct PrivateReviewModel {
        review_url: String,
    }

    impl Respond for PrivateReviewModel {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap_or_default();
            match last_tool_result(&body) {
                None => ollama_tool_call(
                    "web_fetch",
                    serde_json::json!({"url": self.review_url}),
                ),
                Some(result) if result.contains(MCP_RESULT) => ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "message": {"content": "Review evidence loaded through the imported connector."}
                    })),
                Some(result)
                    if result.contains("Authenticated-source recovery")
                        && result.contains(REVIEW_TOOL) =>
                {
                    ollama_tool_call(
                        "tool_search",
                        serde_json::json!({"query": "authenticated code review"}),
                    )
                }
                Some(result) if result.contains("Tools matching") && result.contains(REVIEW_TOOL) => {
                    ollama_tool_call(
                        REVIEW_TOOL,
                        serde_json::json!({"url": self.review_url}),
                    )
                }
                Some(_) => ollama_tool_call(
                    "run_command",
                    serde_json::json!({"command": "curl -fsSL private-review-url"}),
                ),
            }
        }
    }

    /// The production client pool behind the core loop's dependency-cycle seam.
    struct LiveImportedMcp {
        toolset: McpToolset,
    }

    #[async_trait::async_trait]
    impl McpTools for LiveImportedMcp {
        fn handles(&self, name: &str) -> bool {
            self.toolset.handles(name)
        }

        fn tool_defs(&self) -> Vec<serde_json::Value> {
            self.toolset.tool_defs()
        }

        async fn call(&mut self, leased: &LeasedMcpCall<'_>) -> String {
            self.toolset.call(leased.tool(), leased.args()).await
        }
    }

    async fn mount_review_mcp(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/reviews/42"))
            .respond_with(ResponseTemplate::new(401))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"initialize\""))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("Mcp-Session-Id", "composed-session")
                    .set_body_json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-03-26",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "review-source", "version": "1"}
                        }
                    })),
            )
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                "\"method\":\"notifications/initialized\"",
            ))
            .and(header("mcp-session-id", "composed-session"))
            .respond_with(ResponseTemplate::new(202))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/list\""))
            .and(header("mcp-session-id", "composed-session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "result": {
                    "tools": [{
                        "name": "get_review",
                        "description": "Get an authenticated code review from its URL.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"url": {"type": "string"}},
                            "required": ["url"]
                        }
                    }]
                }
            })))
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains("\"method\":\"tools/call\""))
            .and(body_string_contains("\"name\":\"get_review\""))
            .and(header("mcp-session-id", "composed-session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "result": {
                    "content": [{"type": "text", "text": MCP_RESULT}]
                }
            })))
            .mount(server)
            .await;
    }

    /// Ground the entire field regression in one process-level scenario:
    /// Claude config adoption, persisted Newt discovery, streamable-HTTP MCP
    /// handshake/list/call, and the production agent loop's private-URL
    /// `web_fetch` -> `tool_search` -> namespaced connector recovery.
    #[ignore = "real subprocess/filesystem/socket UAT; run in mcp-import-real workflow"]
    #[serial_test::serial(real_fs)]
    #[tokio::test]
    async fn uat_imported_mcp_recovers_a_private_review_without_shell_or_operator_setup() {
        let sb = sandbox();
        let mcp_server = MockServer::start().await;
        mount_review_mcp(&mcp_server).await;
        let review_url = format!("{}/reviews/42", mcp_server.uri());
        let mcp_url = format!("{}/mcp", mcp_server.uri());

        std::fs::write(
            sb.home.join(".claude.json"),
            serde_json::json!({
                "mcpServers": {
                    "review-source": {
                        "type": "http",
                        "url": mcp_url
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        newt(&sb)
            .args([
                "mcp",
                "import",
                "--from-claude",
                "--name",
                "review-source",
                "--grant-net",
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("Imported 1 MCP server(s)"));

        let config_path = sb.config_dir.join("config.toml");
        let runtime_config = load_config(&config_path);
        let persisted = &runtime_config.mcp_servers;
        assert_eq!(persisted.len(), 1, "one connector persisted");
        assert_eq!(persisted[0].name, "review-source");
        assert_eq!(persisted[0].url.as_deref(), Some(mcp_url.as_str()));

        let discovered = newt_core::mcp::discover(persisted, None, Some(&sb.home), &sb.cwd);
        assert_eq!(discovered.len(), 1, "persisted connector wins discovery");
        assert_eq!(discovered[0].name, "review-source");
        assert_eq!(
            discovered[0].trust,
            newt_core::mcp::McpTrust::Trusted,
            "adoption promotes only the sanitized persisted copy to Newt trust"
        );
        assert_eq!(
            runtime_config
                .tui
                .as_ref()
                .expect("grant config")
                .permissions
                .net,
            vec!["127.0.0.1"],
            "grant-net is host-scoped"
        );

        // Exercise the same resolved config inputs and permission lowering as
        // production, then execute the real initialize + tools/list exchange.
        let _discovery_env = DiscoveryEnv::install(&sb);
        let workspace = sb.cwd.to_str().unwrap();
        let caveats = runtime_config
            .tui
            .as_ref()
            .expect("grant config")
            .permissions
            .to_caveats(workspace);
        let toolset =
            McpToolset::connect(workspace, &runtime_config.mcp_servers, true, &caveats).await;
        assert_eq!(toolset.summary(), vec![("review-source".to_string(), 1)]);
        assert!(
            toolset.tool_defs()[0].get("_meta").is_none(),
            "ordinary imported MCPs need no Newt-specific resource metadata"
        );
        assert!(
            toolset.handles(REVIEW_TOOL),
            "hyphenated imported server is exposed under its canonical namespace"
        );
        let mut mcp = LiveImportedMcp { toolset };

        let model = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(PrivateReviewModel {
                review_url: review_url.clone(),
            })
            .mount(&model)
            .await;

        let task = format!("Give me a review of {review_url}");
        let intake = PromptIntake::analyze(&task);
        let messages = vec![
            MemMessage::system("Use connected authenticated sources for private URLs."),
            MemMessage::user(task.clone()),
        ];
        let persona_tools = vec![
            "web_fetch".to_string(),
            "tool_search".to_string(),
            REVIEW_TOOL.to_string(),
        ];
        let mut events: Vec<ToolEvent> = Vec::new();

        let (reply, _, _, hallucinations) = newt_core::chat_complete(
            ChatCtx {
                rewrites_history: true,
                url: &model.uri(),
                model: "composed-private-review-model",
                kind: BackendKind::Ollama,
                emits_leading_reasoning: false,
                api_key: None,
                messages: &messages,
                task: &task,
                workspace: sb.cwd.to_str().unwrap(),
                color: false,
                markdown: false,
                tool_offload: false,
                spill_store: None,
                disclosure: None,
                compaction_store: None,
                scratchpad: false,
                scratchpad_store: None,
                code_search: None,
                where_is: None,
                nav: None,
                exposure: Default::default(),
                experience_store: None,
                step_ledger: None,
                caveats: &caveats,
                persona_tools: Some(&persona_tools),
                cognition: None,
                chat_completions_capability: Default::default(),
                reasoning_replay_scope: newt_core::model_card::ReasoningReplayScope::Never,
                max_tool_rounds: 6,
                narration_nudge_cap: 1,
                action_nudges: true,
                prompt_disposition: intake.disposition(),
                prompt_intake: None,
                workflow_grace_rounds: 0,
                tool_output_lines: 20,
                debug: false,
                trace: false,
                num_ctx: None,
                input_ceiling_pct: 80,
                low_budget_pct: 15,
                connect_timeout_secs: 5,
                inference_timeout_secs: 30,
                mid_loop_trim_threshold: 40,
                compaction_trigger_policy: CompactionTriggerPolicy::HeadroomAware,
                mid_loop_trim_tokens: None,
                max_ok_input: None,
                build_check_cmd: None,
                safe_context: None,
                recover_cw_400: None,
                note_sink: None,
                note_nudge: None,
                recall_source: None,
                memory_source: None,
                summarizer: None,
                compress_state: None,
                tool_events: Some(&mut events),
                phantom_reaches: None,
                end_reason: None,
                solve_obs: None,
                permission_gate: None,
                on_round_usage: None,
                estimate_ratio: None,
                estimation: newt_core::tokens::TokenEstimation::default(),
                summary_input_cap_floor_chars: 8_192,
                exec_floor: None,
                write_ledger: None,
                attribution: None,
                cancel: None,
                live_tool_output: None,
                completed_spill_renderer: None,
                git_tool: None,
                crew_runner: None,
                operating_mode_control: None,
                plan_mode_control: None,
                steering: None,
            },
            &mut mcp,
        )
        .await
        .expect("private review recovers through the imported live MCP");

        let executed: Vec<&str> = events.iter().map(|event| event.tool.as_str()).collect();
        assert_eq!(
            executed,
            vec!["web_fetch", "tool_search", REVIEW_TOOL],
            "raw fetch must recover through discovery and the imported connector: {events:?}"
        );
        assert!(
            !executed.contains(&"run_command") && !executed.contains(&"request_user_input"),
            "recovery must not fall back to shell or operator setup: {events:?}"
        );
        assert_eq!(hallucinations, 0, "all three called tools are real");
        assert!(
            reply.contains("imported connector"),
            "final answer: {reply}"
        );

        let model_wire = model
            .received_requests()
            .await
            .expect("model requests recorded")
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            model_wire.contains("error: web_fetch returned HTTP 401"),
            "the raw-fetch authentication failure must remain intact: {model_wire}"
        );
        assert!(
            model_wire.contains("Authenticated-source recovery"),
            "the connected MCP route must reach the next inference round: {model_wire}"
        );

        let mcp_requests = mcp_server
            .received_requests()
            .await
            .expect("MCP requests recorded");
        let methods: Vec<String> = mcp_requests
            .iter()
            .filter_map(|request| {
                serde_json::from_slice::<serde_json::Value>(&request.body)
                    .ok()?
                    .get("method")?
                    .as_str()
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize".to_string(),
                "notifications/initialized".to_string(),
                "tools/list".to_string(),
                "tools/call".to_string()
            ],
            "the production client must initialize, discover, and call the imported tool"
        );
        let call = mcp_requests
            .iter()
            .filter_map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).ok())
            .find(|request| request["method"] == "tools/call")
            .expect("one live tools/call request");
        assert_eq!(call["params"]["arguments"]["url"], review_url);
    }
}
