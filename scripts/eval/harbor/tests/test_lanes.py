"""Regression: the OCAP-off lane must opt in explicitly.

Since #1582 a plain ``newt solve`` is confined. The adapter used to append
nothing for ``NEWT_BENCH_OCAP`` unset/off, so every "off" run after 2026-08-07
measured the confined lane and recorded ``ocap: on`` in its solve_result.
Run: PYTHONPATH=scripts/eval/harbor python -m unittest scripts/eval/harbor/tests/test_lanes.py
"""

import unittest

import newt_agent
from newt_agent import _lane_flag


class LaneFlag(unittest.TestCase):
    def test_off_lane_opts_in_to_unsafe_host_exec(self):
        for raw in ("", "off", "OFF", " anything "):
            self.assertEqual(_lane_flag(raw), " --unsafe-host-exec", raw)

    def test_on_lane_is_confined(self):
        for raw in ("on", "ON", " on "):
            self.assertEqual(_lane_flag(raw), " --confined", raw)

    def test_lanes_never_collapse(self):
        self.assertNotEqual(_lane_flag("off"), _lane_flag("on"))


class BenchWriteRoots(unittest.TestCase):
    """The confined lane's fence no longer includes the system roots or /home by
    default (a default must be safe on a developer host). Disposable bench
    containers need package installs (#1487), so the adapter ASKS for them."""

    ROOTS = "/usr:/usr/local:/var:/etc:/opt:/root:/home"

    def test_confined_lane_requests_the_broad_roots_and_off_lane_does_not(self):
        try:
            newt_agent._OCAP = "on"
            self.assertIn(f"NEWT_WRITE_PATHS={self.ROOTS} ", newt_agent._container_env_prefix())
            newt_agent._OCAP = ""
            self.assertNotIn("NEWT_WRITE_PATHS", newt_agent._container_env_prefix())
        finally:
            newt_agent._OCAP = ""


class ModelDigest(unittest.TestCase):
    """#2318: a declared digest reaches the container as NEWT_MODEL_DIGEST, and
    none is invented when the operator declared nothing."""

    def test_declared_digest_is_passed_and_absent_is_absent(self):
        try:
            newt_agent._MODEL_DIGEST = "sha256:abc"
            self.assertIn("NEWT_MODEL_DIGEST=sha256:abc ", newt_agent._container_env_prefix())
            newt_agent._MODEL_DIGEST = ""
            self.assertNotIn("NEWT_MODEL_DIGEST", newt_agent._container_env_prefix())
        finally:
            newt_agent._MODEL_DIGEST = ""


class RequiredFeatures(unittest.TestCase):
    """#2318/#2356: a treatment's `requires` becomes `--require-feature` on the
    built newt command, so a default-off feature that cannot be supplied refuses
    before inference; the baseline passes no flag."""

    def test_declared_requirements_become_flags_and_none_adds_none(self):
        try:
            newt_agent._REQUIRE_FEATURES = "scratchpad,crew"
            command = newt_agent._solve_command("/app")
            self.assertIn(" --require-feature scratchpad --require-feature crew", command)
            newt_agent._REQUIRE_FEATURES = ""
            self.assertNotIn("--require-feature", newt_agent._solve_command("/app"))
        finally:
            newt_agent._REQUIRE_FEATURES = ""


class VerifyOutcomes(unittest.TestCase):
    """#2315: the result-aware verification treatment reaches the container, and
    stays absent when the operator did not ask for it (a switch nothing sets is
    the #1943 defect)."""

    def test_opt_in_is_injected_and_absent_is_absent(self):
        try:
            newt_agent._VERIFY_OUTCOMES = "1"
            self.assertIn("NEWT_VERIFY_OUTCOMES=1 ", newt_agent._container_env_prefix())
            newt_agent._VERIFY_OUTCOMES = ""
            self.assertNotIn("NEWT_VERIFY_OUTCOMES", newt_agent._container_env_prefix())
        finally:
            newt_agent._VERIFY_OUTCOMES = ""


class SelfVerifyAblation(unittest.TestCase):
    """newt's self-verify gate is ON by default (#1961), so the baseline already runs
    it and =1 changes nothing. The ablation is =0, which must reach the container."""

    def test_off_is_injected_as_zero_and_absent_is_absent(self):
        try:
            for value, injected in (("0", "NEWT_SELF_VERIFY=0 "), ("off", "NEWT_SELF_VERIFY=0 "),
                                    ("false", "NEWT_SELF_VERIFY=0 "), ("1", "NEWT_SELF_VERIFY=1 ")):
                newt_agent._SELF_VERIFY = value
                self.assertIn(injected, newt_agent._container_env_prefix(), value)
            newt_agent._SELF_VERIFY = ""
            self.assertNotIn("NEWT_SELF_VERIFY", newt_agent._container_env_prefix())
        finally:
            newt_agent._SELF_VERIFY = ""


if __name__ == "__main__":
    unittest.main()
