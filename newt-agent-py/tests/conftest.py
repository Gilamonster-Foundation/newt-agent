"""Shared pytest configuration for the newt-agent-py test suite."""

from __future__ import annotations

import pytest


@pytest.fixture
def newt():
    """Import the umbrella module once per test (cheap; native cdylib)."""
    import newt_agent

    return newt_agent
