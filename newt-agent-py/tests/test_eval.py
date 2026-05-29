"""Smoke tests for newt_agent.eval."""

from __future__ import annotations

import tempfile
from pathlib import Path

import newt_agent.eval as evalmod
from newt_agent.acp_worker import TaskReply


def _write_case(dir: Path, name: str, body: str) -> Path:
    case_dir = dir / name
    (case_dir / "workspace").mkdir(parents=True)
    (case_dir / "case.toml").write_text(body)
    return case_dir


def test_testcase_load_dir_roundtrips() -> None:
    with tempfile.TemporaryDirectory() as d:
        body = """
name = "demo"
description = "demo case"
language = "rust"
prompt = "do a thing"
evaluators = ["diff_nonempty"]
expected_patterns = ["hello"]

[mock_response]
content = "diff goes here"
"""
        case_dir = _write_case(Path(d), "001-demo", body)
        case = evalmod.TestCase.load_dir(str(case_dir))
        assert case.name == "demo"
        assert case.is_rust()
        assert case.evaluators == ["diff_nonempty"]
        assert case.expected_patterns == ["hello"]
        assert case.mock_response.content == "diff goes here"


def test_load_all_sorts_by_name() -> None:
    with tempfile.TemporaryDirectory() as d:
        for n in ["002-bravo", "001-alpha"]:
            _write_case(
                Path(d),
                n,
                f"""
name = "{n}"
description = ""
language = "rust"
prompt = ""
evaluators = []

[mock_response]
content = ""
""",
            )
        cases = evalmod.TestCase.load_all(d)
        assert [c.name for c in cases] == ["001-alpha", "002-bravo"]


def test_eval_result_pass_fail_helpers() -> None:
    p = evalmod.EvalResult.pass_("ev", "ok")
    assert p.passed and p.score == 1.0
    f = evalmod.EvalResult.fail("ev", "boom")
    assert not f.passed and f.score == 0.0


def test_diff_nonempty_evaluator_passes_on_diff() -> None:
    case = _make_inline_case()
    reply = TaskReply("test-model", "content", "+real\n", False)
    ctx = evalmod.EvalContext(case, "/tmp", "/tmp", reply)
    ev = evalmod.DiffNonemptyEvaluator()
    res = ev.evaluate(ctx)
    assert res.passed


def test_diff_nonempty_evaluator_fails_on_empty() -> None:
    case = _make_inline_case()
    reply = TaskReply("test-model", "content", "", False)
    ctx = evalmod.EvalContext(case, "/tmp", "/tmp", reply)
    ev = evalmod.DiffNonemptyEvaluator()
    res = ev.evaluate(ctx)
    assert not res.passed


def test_default_evaluator_names_includes_five() -> None:
    names = evalmod.default_evaluator_names()
    assert set(names) == {
        "diff_nonempty",
        "diff_applies",
        "rust_compiles",
        "tests_pass",
        "pattern_match",
    }


def test_evaluator_known() -> None:
    assert evalmod.evaluator_known("diff_nonempty")
    assert not evalmod.evaluator_known("nope")


def test_runner_config_builders() -> None:
    cfg = evalmod.RunnerConfig("/tmp/newt")
    assert cfg.worker_bin == "/tmp/newt"
    assert cfg.mock_endpoint is None
    cfg.with_mock_endpoint("http://127.0.0.1:8080")
    cfg.with_model("llama3.1:8b")
    cfg.with_timeout_ms(5000)
    cfg.with_coder_mode(True)
    assert cfg.mock_endpoint == "http://127.0.0.1:8080"
    assert cfg.model_override == "llama3.1:8b"
    assert cfg.timeout_ms == 5000
    assert cfg.coder_mode


def test_scorecard_renders_table() -> None:
    s = evalmod.Scorecard()
    s.push("case-a", [evalmod.EvalResult.pass_("diff_nonempty", "ok")])
    s.push("case-b", [evalmod.EvalResult.fail("diff_nonempty", "boom")])
    table = s.render_table()
    assert "case-a" in table
    assert "case-b" in table
    assert "FAIL" in table


def _make_inline_case() -> "evalmod.TestCase":
    # Load a one-off case from disk so the TestCase has a valid `case_dir`.
    import tempfile

    d = tempfile.mkdtemp()
    case_dir = _write_case(
        Path(d),
        "inline",
        """
name = "inline"
description = ""
language = "rust"
prompt = ""
evaluators = []

[mock_response]
content = ""
""",
    )
    return evalmod.TestCase.load_dir(str(case_dir))
