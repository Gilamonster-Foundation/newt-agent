"""Smoke tests for newt_agent.core."""

from __future__ import annotations

import pytest

import newt_agent.core as core


def test_router_classifies_short_prompt_as_fast() -> None:
    router = core.Router()
    assert router.classify("rename foo to bar") == core.Tier.Fast


def test_router_review_keyword_routes_review() -> None:
    router = core.Router()
    assert router.classify("review this PR") == core.Tier.Review


def test_router_refactor_keyword_routes_complex() -> None:
    router = core.Router()
    assert router.classify("refactor the auth middleware to use traits") == core.Tier.Complex


def test_router_override_always_returns_its_tier() -> None:
    r = core.Router.with_override(core.Tier.Standard)
    assert r.classify("anything") == core.Tier.Standard


def test_classification_detailed_has_reasons() -> None:
    c = core.Router().classify_detailed("review this diff")
    assert c.tier == core.Tier.Review
    assert 0.0 <= c.confidence <= 1.0
    assert any("review" in r for r in c.reasons)


def test_tier_parse_canonical_names() -> None:
    assert core.Tier.parse("FAST") == core.Tier.Fast
    assert core.Tier.parse("standard") == core.Tier.Standard
    assert core.Tier.parse("Complex") == core.Tier.Complex
    assert core.Tier.parse("review") == core.Tier.Review


def test_tier_parse_rejects_unknown() -> None:
    with pytest.raises(Exception):
        core.Tier.parse("BOGUS")


def test_session_id_roundtrip() -> None:
    sid = core.SessionId()
    s = str(sid)
    parsed = core.SessionId.parse(s)
    assert parsed == sid


def test_session_id_invalid_string_rejected() -> None:
    with pytest.raises(Exception):
        core.SessionId.parse("not-a-uuid")


def test_model_id_str_and_hash() -> None:
    a = core.ModelId("llama3.1:8b")
    b = core.ModelId("llama3.1:8b")
    c = core.ModelId("qwen2.5-coder:32b")
    assert a == b
    assert a != c
    assert str(a) == "llama3.1:8b"
    assert hash(a) == hash(b)


def test_config_defaults_are_sensible() -> None:
    cfg = core.Config()
    assert len(cfg.backends) == 1
    assert cfg.backends[0].name == "ollama"
    assert cfg.backends[0].model == "llama3.1:8b"
    assert core.Tier.Fast in cfg.backends[0].tiers


def test_newt_error_exported() -> None:
    assert hasattr(core, "NewtError")
