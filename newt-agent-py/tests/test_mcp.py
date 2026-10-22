"""Smoke tests for newt_agent.mcp."""

from __future__ import annotations

import json

import newt_agent.mcp as mcp


def test_default_tool_definitions_lists_the_builtins() -> None:
    raw = mcp.default_tool_definitions()
    tools = json.loads(raw)
    names = {t["name"] for t in tools}
    assert {
        "read_file",
        "write_file",
        "edit_file",
        "list_dir",
        "find",
        "grep",
        "goal_run",
    } <= names
    # The file tools carry newt-core's catalog description and schema, same
    # as `tools/list` (#2980 review) — not a bare name.
    for tool in tools:
        assert tool["description"], tool["name"]
        if tool["name"] != "goal_run":
            assert tool["inputSchema"]["type"] == "object", tool["name"]


def test_mcp_server_register_and_handle() -> None:
    server = mcp.McpServer()
    assert server.registered_methods() == []

    def echo(params_json: str):
        return {"echo": params_json}

    server.register("echo", echo)
    assert "echo" in server.registered_methods()

    result = server.handle("echo", '{"k": 1}')
    assert result is not None
    parsed = json.loads(result)
    assert parsed == {"echo": '{"k": 1}'}


def test_mcp_server_unknown_method_returns_none() -> None:
    server = mcp.McpServer()
    assert server.handle("nope", "{}") is None


def test_mcp_server_repr() -> None:
    server = mcp.McpServer()
    server.register("foo", lambda _: "bar")
    assert "McpServer(methods=1)" in repr(server)
