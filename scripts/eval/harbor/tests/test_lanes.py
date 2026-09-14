"""Regression: the OCAP-off lane must opt in explicitly.

Since #1582 a plain ``newt solve`` is confined. The adapter used to append
nothing for ``NEWT_BENCH_OCAP`` unset/off, so every "off" run after 2026-08-07
measured the confined lane and recorded ``ocap: on`` in its solve_result.
Run: PYTHONPATH=scripts/eval/harbor python -m unittest scripts/eval/harbor/tests/test_lanes.py
"""

import unittest

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


if __name__ == "__main__":
    unittest.main()
