use super::*;

#[tokio::test]
async fn initialize_returns_protocol_version_and_name() {
    let resp = rpc(&serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    }))
    .await;

    let result = &resp["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "newt-mcp-data");
    assert!(result["capabilities"]["tools"].is_object());
}
#[tokio::test]
async fn tools_list_returns_the_sql_and_kernel_tools() {
    let resp = rpc(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
    }))
    .await;

    let tools = resp["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    // The four SQL tools (21.2), the two live-kernel tools (21.3), the three
    // notebook tools (21.4), and the two dataframe-introspection tools (21.5).
    for expected in [
        "sql_ingest_csv",
        "sql_query",
        "sql_summarize",
        "sql_list_tables",
        "kernel_attach",
        "run_cell",
        "notebook_read",
        "notebook_insert_cell",
        "notebook_persist_executed_cell",
        "list_dataframes",
        "inspect_dataframe",
    ] {
        assert!(names.contains(&expected), "tools/list missing {expected}");
    }
    assert_eq!(tools.len(), 11, "expected exactly 11 tools, got {names:?}");

    // Every tool carries an inputSchema object.
    for tool in tools {
        assert!(
            tool["inputSchema"].is_object(),
            "tool {} missing inputSchema",
            tool["name"]
        );
    }
    // kernel_attach requires `url`; run_cell requires `code` (persist_to is
    // optional, so it must NOT be in `required`).
    let attach = tools.iter().find(|t| t["name"] == "kernel_attach").unwrap();
    assert_eq!(
        attach["inputSchema"]["required"],
        serde_json::json!(["url"])
    );
    let run = tools.iter().find(|t| t["name"] == "run_cell").unwrap();
    assert_eq!(run["inputSchema"]["required"], serde_json::json!(["code"]));
    // run_cell advertises the optional persist_to property.
    assert!(
        run["inputSchema"]["properties"]["persist_to"].is_object(),
        "run_cell must advertise the optional persist_to argument"
    );
    // notebook_insert_cell requires path + source; persist requires outputs too.
    let insert = tools
        .iter()
        .find(|t| t["name"] == "notebook_insert_cell")
        .unwrap();
    assert_eq!(
        insert["inputSchema"]["required"],
        serde_json::json!(["path", "source"])
    );
    let persist = tools
        .iter()
        .find(|t| t["name"] == "notebook_persist_executed_cell")
        .unwrap();
    assert_eq!(
        persist["inputSchema"]["required"],
        serde_json::json!(["path", "source", "outputs"])
    );
    // list_dataframes requires nothing; inspect_dataframe requires `name`
    // (head is optional, so it must NOT be in `required`).
    let list_df = tools
        .iter()
        .find(|t| t["name"] == "list_dataframes")
        .unwrap();
    assert!(
        list_df["inputSchema"]["required"].is_null(),
        "list_dataframes must have no required args"
    );
    let inspect = tools
        .iter()
        .find(|t| t["name"] == "inspect_dataframe")
        .unwrap();
    assert_eq!(
        inspect["inputSchema"]["required"],
        serde_json::json!(["name"])
    );
    assert!(
        inspect["inputSchema"]["properties"]["head"].is_object(),
        "inspect_dataframe must advertise the optional head argument"
    );
}
/// An unknown tool name stays a `-32603` transport fault, matching the
/// `other =>` arm of `newt-mcp-server`'s `tools/call` dispatch.
#[tokio::test]
async fn unknown_tool_returns_transport_error() {
    let resp = rpc(&call(40, "nonexistent_tool", serde_json::json!({}))).await;
    assert!(
        resp["error"].is_object(),
        "expected transport error: {resp}"
    );
    assert_eq!(resp["error"]["code"], -32603);
    assert!(resp["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown tool"));
}
