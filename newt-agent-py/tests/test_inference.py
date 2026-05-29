"""Smoke tests for newt_agent.inference.

The async surfaces require a running asyncio loop; pytest-asyncio is
configured via `asyncio_mode = "auto"` in the root pyproject.

We deliberately do NOT exercise real Ollama / vLLM HTTP — those
tests live in the Rust side with `wiremock`. The Python suite confirms
the bindings *exist*, can be constructed, and their async return
shape is awaitable.
"""

from __future__ import annotations

import pytest

import newt_agent.inference as inference


def test_chat_request_builder_chain() -> None:
    req = inference.ChatRequest()
    req.system("You are helpful.")
    req.user("Hello")
    req.with_max_tokens(256)
    assert len(req.messages) == 2
    assert req.messages[0].role == "system"
    assert req.messages[1].role == "user"
    assert req.max_tokens == 256


def test_chat_reply_construction_and_audit_string() -> None:
    reply = inference.ChatReply("hello", "llama3.1:8b")
    assert reply.content == "hello"
    assert reply.model_id == "llama3.1:8b"
    assert reply.audit_string("ollama-local") == "backend=ollama-local model_id=llama3.1:8b"


def test_message_classmethods() -> None:
    m = inference.Message.system("sys")
    assert m.role == "system" and m.content == "sys"
    m = inference.Message.user("u")
    assert m.role == "user"
    m = inference.Message.assistant("a")
    assert m.role == "assistant"


def test_local_ollama_backend_construct() -> None:
    backend = inference.LocalOllamaBackend("http://127.0.0.1:11434", "llama3.1:8b")
    assert backend.endpoint() == "http://127.0.0.1:11434"
    assert backend.model_id() == "llama3.1:8b"
    assert backend.name() == "ollama-local"


def test_local_ollama_default_endpoints_include_localhost() -> None:
    eps = inference.LocalOllamaBackend.default_endpoints()
    assert any("127.0.0.1" in ep for ep in eps)


def test_local_vllm_backend_construct() -> None:
    backend = inference.LocalVllmBackend("http://127.0.0.1:8000", "meta/llama")
    assert backend.endpoint() == "http://127.0.0.1:8000"
    assert backend.name() == "vllm-local"


async def test_backend_registry_register_and_names() -> None:
    registry = inference.BackendRegistry()
    backend = inference.LocalOllamaBackend("http://127.0.0.1:11434", "llama3.1:8b")
    await registry.register_ollama(backend)
    assert await registry.len() == 1
    names = await registry.names()
    assert names == ["ollama-local"]


async def test_local_ollama_complete_against_unreachable_endpoint_raises() -> None:
    # Bind to an unreachable port so we exercise the awaitable path
    # without standing up a real server. The backend retries and
    # eventually raises; we just check we get an exception.
    backend = inference.LocalOllamaBackend("http://127.0.0.1:1", "llama3.1:8b")
    req = inference.ChatRequest()
    req.user("hello")
    with pytest.raises(Exception):
        await backend.complete(req)
