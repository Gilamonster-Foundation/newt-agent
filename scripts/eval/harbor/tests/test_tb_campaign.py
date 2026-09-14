"""tb_campaign.py: claims are read from each harness's own log, an unreadable
claim stays unknown instead of becoming "not claimed", and a trial that ran
another model or was never graded stays visible in the summary.
Run: PYTHONPATH=scripts/eval/harbor python -m unittest discover scripts/eval/harbor/tests
"""

import json
import tomllib
import unittest
from pathlib import Path

from tb_campaign import (
    PINNED, build_cell, codex_claim, error_cause, harness_evidence, load_treatment, newcombe, newt_claim, observed,
    pair, pair_report, pi_claim, pin_extend, pin_mismatch, render_profile, summarize, wilson,
)

HARBOR = Path(__file__).resolve().parent.parent
BASE = ('default_backend = "b"\n[[backends]]\nname = "b"\n'
        'endpoint = "http://inference.invalid:8080"\nmodel = "old"\nkind = "openai"\n')


def jl(*records):
    return [json.dumps(r) for r in records]


def row(**kw):
    base = dict(state="graded", reward=0.0, resolved=False, error_cause=None, inference_failure=None,
                claimed_done=True, exception=None, model_effective="m", tokens_in=None, tokens_out=None,
                agent_s=None, max_request_output_tokens=None)
    return {**base, **kw}


class Claims(unittest.TestCase):
    def test_newt_uses_the_last_contract_outcome(self):
        lines = jl({"kind": "solve_result", "status": "completed"}, {"outcome": "timeout"})
        self.assertEqual(newt_claim(lines), (False, "contract outcome=timeout"))
        self.assertIsNone(newt_claim(jl({"kind": "chat_completion_finish"})))

    def test_pi_claims_only_on_a_final_stop(self):
        def end(reason):
            return {"type": "agent_end", "messages": [
                {"role": "assistant", "stopReason": "toolUse"}, {"role": "user"},
                {"role": "assistant", "stopReason": reason}]}

        self.assertEqual(pi_claim(jl(end("stop")))[0], True)
        self.assertEqual(pi_claim(jl(end("length")))[0], False)
        self.assertIsNone(pi_claim(jl({"type": "turn_end"}) + ["not json"]))
        # auto-retry: an errored first attempt, then a finished one — the last agent_end decides
        self.assertEqual(pi_claim(jl(end("error"), end("stop")))[0], True)

    def test_codex_turn_outcome(self):
        self.assertEqual(codex_claim(jl({"type": "turn.started"}, {"type": "turn.completed"}))[0], True)
        self.assertEqual(codex_claim(jl({"type": "turn.failed"}))[0], False)
        self.assertIsNone(codex_claim(jl({"type": "thread.started"})))


class ErrorCause(unittest.TestCase):
    NZ = "NonZeroAgentExitCodeError"

    def test_runaway_truncated_stream_after_replies_is_agent(self):
        # circuit-fibsqrt__sQVNJH6 (smart-ab-8 baseline): 13 completions, then newt refused the stream.
        lines = jl(*[{"kind": "chat_completion_finish", "finish_reason": "tool_calls"}] * 13,
                   {"kind": "solve_result", "error": "streamed tool batch did not finish with tool_calls"})
        replies, err = harness_evidence("newt", lines)
        self.assertEqual(replies, 13)
        self.assertEqual(error_cause(self.NZ, 1, err, None, replies), "agent")

    def test_apparatus_evidence_is_infra_even_after_replies(self):
        self.assertEqual(error_cause(None, None, None, "auto_retry_end success=false: Connection error.", 0), "infra")
        self.assertEqual(error_cause(self.NZ, 1, "inference endpoint: error sending request", None, 4), "infra")
        self.assertEqual(error_cause(self.NZ, 137, None, None, 9), "infra")
        self.assertEqual(error_cause("EnvironmentStartTimeoutError", None, None, None, 0), "infra")
        _, err = harness_evidence("codex", jl({"type": "turn.failed", "error": {"message": "stream disconnected before completion"}}))
        self.assertEqual(error_cause(self.NZ, 1, err, None, 2), "infra")

    def test_unprovable_is_unknown_and_no_exception_is_none(self):
        self.assertEqual(error_cause("AgentTimeoutError", None, None, None, 0), "unknown")
        self.assertEqual(error_cause("AgentTimeoutError", None, None, None, 5), "agent")
        self.assertIsNone(error_cause(None, None, None, None, 7))


class Summary(unittest.TestCase):
    def test_wilson(self):
        lo, hi = wilson(0, 1)
        self.assertEqual(lo, 0.0)
        self.assertAlmostEqual(hi, 0.7935, places=3)
        self.assertIsNone(wilson(0, 0))

    def test_unknown_claims_and_missing_grades_are_not_folded_in(self):
        rows = [
            row(resolved=True, reward=1.0),                                   # true completion
            row(),                                                            # false completion
            row(claimed_done=False, resolved=True, reward=1.0),               # false incomplete
            row(claimed_done=None),                                           # unrecoverable claim
            row(state="error", reward=None, exception="AgentSetupTimeoutError", error_cause="infra"),
            row(model_effective="other"),                                     # ran another model
            row(state="error", error_cause="infra", inference_failure="auto_retry_end"),  # pi exit 0
            row(state="error", claimed_done=False, exception="AgentTimeoutError", error_cause="agent",
                max_request_output_tokens=96345),
        ]
        s = summarize({"expected": 9, "model": "m"}, rows)
        self.assertEqual((s["expected"], s["observed"], s["graded"], s["errors"]), (9, 8, 5, 3))
        self.assertEqual(s["claimed"], 3)  # infra trials never count as a claim outcome
        self.assertEqual((s["false_completions"], s["false_incompletes"]), (2, 1))
        self.assertEqual((s["unrecoverable_claims"], s["exceptions"], s["model_mismatches"]), (1, 2, 1))
        self.assertEqual((s["inference_errors"], s["agent_timeouts"], s["max_request_output"]), (1, 1, 96345))
        self.assertEqual((s["rate_excl"], s["rate_agent_fail"], s["causes"]), ((2, 5), (2, 6), (2, 1, 0)))
        self.assertEqual(s["tokens_in"], (0, 0))  # none known: count 0, not a zero cost

    def test_two_resolve_rates(self):
        rows = [
            row(resolved=True, reward=1.0),
            row(),
            row(state="error", exception="NonZeroAgentExitCodeError", error_cause="agent"),  # runaway, reward 0
            row(state="error", exception="NetworkConnectionError", error_cause="infra"),     # router down
            row(state="error", exception="AgentTimeoutError", error_cause="unknown"),
        ]
        s = summarize({"expected": 5, "model": "m"}, rows)
        self.assertEqual(s["rate_excl"], (1, 2))
        self.assertEqual(s["rate_agent_fail"], (1, 3))
        self.assertEqual(s["causes"], (1, 1, 1))
        self.assertEqual(s["tokens_in"], (0, 0))  # none known: count 0, not a zero cost


class Treatments(unittest.TestCase):
    def test_none_is_the_baseline(self):
        t = load_treatment("none")
        self.assertEqual((t["name"], t["sha256"], t["env"], t["expect"]), ("none", None, {}, {}))
        self.assertEqual(render_profile(t, "m", BASE), BASE.replace('model = "old"', 'model = "m"'))

    def test_committed_treatments_render_to_valid_profiles_without_placeholders(self):
        files = sorted((HARBOR / "treatments").glob("*.toml"))
        self.assertTrue(files)
        for f in files:
            text = render_profile(load_treatment(f), "qwen3-coder_30b", BASE)
            self.assertNotIn("{{", text, f)
            self.assertEqual(tomllib.loads(text)["backends"][0]["model"], "qwen3-coder_30b", f)

    def test_smart_takes_its_endpoint_from_the_local_profile(self):
        doc = tomllib.loads(render_profile(load_treatment(HARBOR / "treatments/smart.toml"), "m", BASE))
        self.assertEqual(doc["smart_harness"]["backend"], {"endpoint": "http://inference.invalid:8080", "model": "m", "kind": "openai"})

    def test_env_outside_the_adapter_knobs_is_refused(self):
        with self.assertRaises(ValueError):
            load_treatment(HARBOR / "tests/fixtures/treatment-bad-env.toml")

    def test_observed_reads_dotted_contract_paths(self):
        expect = {"effective_config.smart_harness": "*", "effective_config.ocap": "off"}
        on = {"outcome": "completed", "effective_config": {"smart_harness": {"enabled": True}, "ocap": "off"}}
        self.assertIs(observed(expect, on), True)
        self.assertIs(observed(expect, {"effective_config": {"ocap": "off"}}), False)
        self.assertIsNone(observed({}, on))       # nothing declared observable
        self.assertIsNone(observed(expect, None))  # no contract record to read

    def test_cell_binding_parses_json_and_counts(self):
        t = load_treatment(HARBOR / "treatments/smart.toml")
        cell = build_cell(t, ["expected", "24", "model_fingerprint_json", '{"size": 1}', "skipped", ""])
        self.assertEqual((cell["expected"], cell["model_fingerprint"], cell["skipped"]), (24, {"size": 1}, None))
        self.assertEqual((cell["treatment"], cell["treatment_env"]), ("smart", {"NEWT_BENCH_SMART": "1"}))
        self.assertEqual(len(cell["treatment_sha256"]), 64)


class Pinning(unittest.TestCase):
    CELL = dict(model="m", harness="newt", task_set_sha256="t", engine="e", ctx_served="131072",
                newt_binary_sha256="b", instrument_commit="c", model_fingerprint={"size": 1, "ftype": "Q4_K"},
                harness_version="0.8.0")

    def test_the_writing_cell_matches_its_own_pin(self):
        self.assertEqual(pin_mismatch(pin_extend({}, dict(self.CELL)), self.CELL), [])

    def test_every_pinned_field_refuses_a_different_cell(self):
        pin = pin_extend({}, dict(self.CELL))
        for k in PINNED:
            self.assertEqual(pin_mismatch(pin, {**self.CELL, k: "other"}), [k])

    def test_a_changed_model_fingerprint_refuses(self):
        pin = pin_extend({}, dict(self.CELL))
        self.assertEqual(pin_mismatch(pin, {**self.CELL, "model_fingerprint": {"size": 2, "ftype": "Q4_K"}}), ["model_fingerprint"])

    def test_a_new_model_extends_the_pin_and_harness_versions_are_pinned_once_seen(self):
        pin = pin_extend({}, dict(self.CELL))
        second = {**self.CELL, "model": "m2", "model_fingerprint": {"size": 9}}
        self.assertEqual(pin_mismatch(pin, second), [])
        pin_extend(pin, second)
        self.assertEqual(pin["models"]["m2"], {"size": 9})
        pi = {**self.CELL, "harness": "pi", "harness_version": None}   # before its first install
        self.assertEqual(pin_mismatch(pin, pi), [])
        pin_extend(pin, {**pi, "harness_version": "0.85.1"})
        self.assertEqual(pin_mismatch(pin, {**pi, "harness_version": "0.86.0"}), ["harness_version"])


def arm(outcomes, **kw):
    """Rows for one arm from {task: [resolved?, ...]}."""
    return [row(task=task, resolved=ok, reward=1.0 if ok else 0.0, **kw)
            for task, oks in outcomes.items() for ok in oks]


class Pairing(unittest.TestCase):
    TASKS = ("a", "b", "c", "d", "e")

    def test_newcombe_matches_a_hand_computed_value(self):
        lo, hi = newcombe(3, 12, 0, 12)
        self.assertAlmostEqual(lo, -0.0411, places=3)
        self.assertAlmostEqual(hi, 0.5323, places=3)

    def test_a_uniform_effect_has_a_tight_interval(self):
        p = pair(arm({t: [False] * 3 for t in self.TASKS}), arm({t: [True] * 3 for t in self.TASKS}), "excl")
        self.assertEqual((p["diff"], p["bootstrap"], p["better"], p["floor"]), (1.0, (1.0, 1.0), 5, False))

    def test_one_correlated_task_widens_the_clustered_interval_beyond_newcombe(self):
        # The treatment wins 3/3 on ONE task and nothing elsewhere: one cluster, not 3 independent wins.
        base = arm({t: [False] * 3 for t in self.TASKS})
        treat = arm({"a": [True] * 3, **{t: [False] * 3 for t in self.TASKS[1:]}})
        p = pair(base, treat, "excl")
        self.assertEqual((p["diff"], p["better"], p["tied"]), (0.2, 1, 4))
        self.assertEqual(p["bootstrap"][0], 0.0)
        self.assertGreater(p["bootstrap"][1], p["newcombe"][1])

    def test_bootstrap_is_reproducible_and_seeded(self):
        base = arm({"a": [True, False, False], "b": [False] * 3, "c": [True] * 3, "d": [False, True, False]})
        treat = arm({"a": [True] * 3, "b": [False, True, False], "c": [True, True, False], "d": [False] * 3})
        self.assertEqual(pair(base, treat, "excl")["bootstrap"], pair(base, treat, "excl")["bootstrap"])

    def test_floor_ceiling_unpaired_and_too_few_tasks_are_named(self):
        p = pair(arm({"a": [False] * 3, "b": [False] * 3}), arm({"a": [False] * 3, "z": [False] * 3}), "excl")
        self.assertEqual((p["diff"], p["floor"], p["unpaired"]), (0.0, True, ["b", "z"]))
        self.assertIsNone(p["bootstrap"])  # 1 paired task: no between-task variance to resample
        self.assertTrue(pair(arm({"a": [True]}), arm({"a": [True]}), "excl")["ceiling"])

    def test_rate_definitions_choose_the_trials(self):
        base = arm({"a": [False, False]})
        treat = arm({"a": [True]}) + [row(task="a", state="error", error_cause="agent")]  # runaway, reward 0
        self.assertEqual(pair(base, treat, "excl")["treat"], (1, 1))
        self.assertEqual(pair(base, treat, "agent_fail")["treat"], (1, 2))

    def test_report_refuses_to_pair_across_a_skipped_or_off_pin_cell(self):
        cells = {
            "m__newt": {"job": "m__newt", "model": "m", "harness": "newt", "treatment": "none"},
            "m__newt__smart": {"job": "m__newt__smart", "model": "m", "harness": "newt", "treatment": "smart",
                               "pin_mismatch": ["model_fingerprint"]},
        }
        self.assertIn("Not paired", pair_report(cells, []))
        cells["m__newt__smart"].pop("pin_mismatch")
        rows = [dict(r, job="m__newt") for r in arm({"a": [False] * 3})] + \
               [dict(r, job="m__newt__smart") for r in arm({"a": [True] * 3})]
        report = pair_report(cells, rows)
        self.assertIn("Δ = 3/3 − 0/3 = +1.00", report)
        self.assertIn("task-clustered paired bootstrap 95% not reported (1 paired tasks; needs 5)", report)


if __name__ == "__main__":
    unittest.main()
