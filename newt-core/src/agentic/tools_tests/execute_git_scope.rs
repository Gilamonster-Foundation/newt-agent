use super::*;

struct GitPresent;
impl crate::agentic::GitTool for GitPresent {
    fn dispatch(
        &self,
        _op: &str,
        _args: &serde_json::Value,
        _caps: &crate::git_caveats::GitCaveats,
        _session: &Caveats,
    ) -> Result<String, String> {
        panic!("catalog discovery must not dispatch Git")
    }
}

#[tokio::test]
async fn scoped_act_git_discovery_does_not_advertise_legacy_operations() {
    let ws = tempfile::tempdir().unwrap();
    for fs_read in [
        Scope::All,
        Scope::only([ws.path().to_string_lossy().into_owned()]),
    ] {
        let scoped = fs_read != Scope::All;
        let caveats = Caveats {
            fs_read,
            ..Caveats::top()
        };
        let out = execute_tool_with_collaborators(
            "tool_search",
            &serde_json::json!({"query": "git"}),
            &ws.path().to_string_lossy(),
            false,
            20,
            &caveats,
            &mut NoMcp,
            ToolCollaborators {
                git_tool: Some(&GitPresent),
                ..Default::default()
            },
            false,
            PromptDisposition::Act,
            None,
        )
        .await
        .expect("legacy fixture has no durable writer")
        .unwrap();
        assert_eq!(out.contains("git — List and count"), scoped, "{out}");
        assert_eq!(out.contains("git — Run a git operation"), !scoped, "{out}");
    }
}

#[test]
fn git_catalog_intersects_read_scope_with_prompt_disposition() {
    for scope in [Scope::All, Scope::none(), Scope::only(["/".to_owned()])] {
        for disposition in [
            PromptDisposition::Act,
            PromptDisposition::Explain,
            PromptDisposition::Research,
            PromptDisposition::Plan,
            PromptDisposition::Ask,
        ] {
            let defs = merged_tool_definitions(
                &NoMcp,
                false,
                false,
                false,
                Some(&scope),
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                false,
            );
            let defs = filter_tools_for_disposition(defs, disposition);
            let git = defs
                .as_array()
                .unwrap()
                .iter()
                .find(|def| def["function"]["name"] == "git");
            if disposition == PromptDisposition::Ask {
                assert!(git.is_none());
            } else {
                let ops = &git.unwrap()["function"]["parameters"]["properties"]["op"]["enum"];
                if scope != Scope::All || disposition != PromptDisposition::Act {
                    assert_eq!(ops, &serde_json::json!(["branch-list"]));
                } else {
                    assert!(ops
                        .as_array()
                        .unwrap()
                        .contains(&serde_json::json!("commit")));
                }
            }
        }
    }
}

#[tokio::test]
async fn scoped_git_denial_cannot_request_a_write_grant_or_dispatch() {
    let ws = tempfile::tempdir().unwrap();
    let caveats = Caveats {
        fs_read: Scope::only([ws.path().to_string_lossy().into_owned()]),
        ..caveats_rw(ws.path())
    };
    let definition = crate::agentic::git_tool_definition();
    let ops = definition["function"]["parameters"]["properties"]["op"]["enum"]
        .as_array()
        .unwrap();
    for op in ops
        .iter()
        .filter_map(serde_json::Value::as_str)
        .filter(|op| *op != "branch-list")
    {
        let mut gate = MockGate::new(true, &caveats);
        let out = run_git_gated_scope(op, &caveats, &mut gate).await;
        assert!(out.contains("scoped fs_read"), "{op}: {out}");
        assert!(
            !is_git_write_denial(&out),
            "scope errors are not grantable writes"
        );
        assert!(gate.asks.is_empty(), "{op}: {:?}", gate.asks);
    }
}

async fn run_git_gated_scope(op: &str, caveats: &Caveats, gate: &mut MockGate) -> String {
    execute_tool_with_collaborators(
        "git",
        &serde_json::json!({"op": op}),
        ".",
        false,
        20,
        caveats,
        &mut NoMcp,
        ToolCollaborators {
            git_tool: Some(&GitPresent),
            permission_gate: Some(gate),
            ..Default::default()
        },
        false,
        PromptDisposition::Act,
        None,
    )
    .await
    .expect("legacy fixture has no durable writer")
    .unwrap()
}
