"""Smoke tests for newt_agent.acp_worker (data types)."""

from __future__ import annotations

import json

import pytest

import newt_agent.acp_worker as acp


def test_task_reply_construct_and_attrs() -> None:
    r = acp.TaskReply("qwen2.5-coder:32b", "content", "+a\n", True)
    assert r.model_id == "qwen2.5-coder:32b"
    assert r.content == "content"
    assert r.diff == "+a\n"
    assert r.diff_applied is True
    assert r.empty_diff is False
    assert r.emission_shape is None


def test_task_reply_with_emission_shape() -> None:
    r = acp.TaskReply("m", "c", "+x\n", True, emission_shape="whole_files")
    assert r.emission_shape == "whole_files"


def test_task_reply_rejects_empty_model_id() -> None:
    with pytest.raises(Exception):
        acp.TaskReply("", "c", "", False)


def test_task_reply_empty_diff_signal() -> None:
    r = acp.TaskReply("m", "c", "", False)
    assert r.empty_diff is True
    r2 = acp.TaskReply("m", "c", "real\nchanges\n", True)
    assert r2.empty_diff is False


def test_task_reply_json_roundtrip() -> None:
    r = acp.TaskReply("m", "c", "+d\n", True, emission_shape="whole_files")
    js = r.to_json()
    parsed = json.loads(js)
    assert parsed["model_id"] == "m"
    assert parsed["emission_shape"] == "whole_files"
    back = acp.TaskReply.from_json(js)
    assert back.model_id == "m"
    assert back.emission_shape == "whole_files"


def test_task_reply_omits_null_emission_shape() -> None:
    r = acp.TaskReply("m", "c", "", False)
    js = r.to_json()
    assert "emission_shape" not in js


def test_session_construct_and_attrs() -> None:
    s = acp.Session("/tmp/ws", coder_enabled=True, model_override="qwen2.5-coder:32b")
    assert s.workspace_path == "/tmp/ws"
    assert s.coder_enabled is True
    assert s.model_override == "qwen2.5-coder:32b"


def test_is_empty_diff_helper() -> None:
    assert acp.is_empty_diff("")
    assert acp.is_empty_diff("   \n")
    assert not acp.is_empty_diff("+real change\n")
