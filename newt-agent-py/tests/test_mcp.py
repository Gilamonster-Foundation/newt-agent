"""Smoke tests for newt_agent.mcp."""

from __future__ import annotations

import json

import newt_agent.mcp as mcp


def test_default_tool_definitions_lists_four() -> None:
    raw = mcp.default_tool_definitions()
    tools = json.loads(raw)
    names = {t["name"] for t in tools}
    assert names == {"code_read", "code_edit", "code_search", "goal_run"}


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
