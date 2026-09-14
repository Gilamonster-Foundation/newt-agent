"""tb_campaign.py: claims are read from each harness's own log, an unreadable
claim stays unknown instead of becoming "not claimed", and a trial that ran
another model or was never graded stays visible in the summary.
Run: PYTHONPATH=scripts/eval/harbor python -m unittest discover scripts/eval/harbor/tests
"""

import json
import unittest

from tb_campaign import codex_claim, newt_claim, pi_claim, summarize, wilson


def jl(*records):
    return [json.dumps(r) for r in records]


def row(**kw):
    base = dict(graded=True, resolved=False, claimed_done=True, exception=None,
                model_effective="m", tokens_in=None, tokens_out=None, agent_s=None,
                max_request_output_tokens=None)
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


class Summary(unittest.TestCase):
    def test_wilson(self):
        lo, hi = wilson(0, 1)
        self.assertEqual(lo, 0.0)
        self.assertAlmostEqual(hi, 0.7935, places=3)
        self.assertIsNone(wilson(0, 0))

    def test_unknown_claims_and_missing_grades_are_not_folded_in(self):
        rows = [
            row(resolved=True),                                   # true completion
            row(),                                                # false completion
            row(claimed_done=False, resolved=True),               # false incomplete
            row(claimed_done=None),                               # unrecoverable claim
            row(graded=False, exception="AgentSetupError"),       # ungraded
            row(model_effective="other"),                         # ran another model
            row(graded=False, state="error"),                     # pi exit-0 inference failure
            row(claimed_done=False, exception="AgentTimeoutError", max_request_output_tokens=96345),
        ]
        s = summarize({"expected": 9, "model": "m"}, rows)
        self.assertEqual((s["expected"], s["observed"], s["graded"]), (9, 8, 6))
        self.assertEqual((s["resolved"], s["claimed"]), (2, 3))
        self.assertEqual((s["false_completions"], s["false_incompletes"]), (2, 1))
        self.assertEqual((s["unrecoverable_claims"], s["exceptions"], s["model_mismatches"]), (1, 2, 1))
        self.assertEqual(s["inference_errors"], 1)
        self.assertEqual((s["agent_timeouts"], s["max_request_output"]), (1, 96345))
        self.assertEqual(s["tokens_in"], (0, 0))  # none known: count 0, not a zero cost


if __name__ == "__main__":
    unittest.main()
