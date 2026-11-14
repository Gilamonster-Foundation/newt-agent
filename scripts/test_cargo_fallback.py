#!/usr/bin/env python3
"""Pure environment decisions: no filesystem, subprocesses, or live cache."""
import unittest

import cargo_wrapper


class CargoFallbackTests(unittest.TestCase):
    def check_env(self, env, probe_ok, expected):
        original = dict(env)
        self.assertEqual(cargo_wrapper.cargo_env(env, probe_ok), expected)
        self.assertEqual(env, original, "caller environment must not be mutated")

    def test_no_wrapper(self):
        self.check_env({"OTHER": "keep"}, False, {"OTHER": "keep"})

    def test_other_wrapper(self):
        self.check_env({"RUSTC_WRAPPER": "other"}, False, {"RUSTC_WRAPPER": "other"})

    def test_healthy_sccache(self):
        self.check_env({"RUSTC_WRAPPER": "sccache"}, True,
                       {"RUSTC_WRAPPER": "sccache", "SCCACHE_IGNORE_SERVER_IO_ERROR": "1"})

    def test_failed_explicit_sccache(self):
        self.check_env({"RUSTC_WRAPPER": "sccache", "OTHER": "keep"}, False,
                       {"RUSTC_WRAPPER": "", "OTHER": "keep", "SCCACHE_IGNORE_SERVER_IO_ERROR": "1"})

    def test_failed_build_env_sccache(self):
        self.check_env({"CARGO_BUILD_RUSTC_WRAPPER": "sccache"}, False,
                       {"CARGO_BUILD_RUSTC_WRAPPER": "sccache", "RUSTC_WRAPPER": "",
                        "SCCACHE_IGNORE_SERVER_IO_ERROR": "1"})

    def test_explicit_empty_override(self):
        env = {"RUSTC_WRAPPER": "", "CARGO_BUILD_RUSTC_WRAPPER": "sccache"}
        self.check_env(env, False, env)

    def test_absolute_sccache_path(self):
        self.check_env({"RUSTC_WRAPPER": "/opt/tools/sccache"}, False,
                       {"RUSTC_WRAPPER": "", "SCCACHE_IGNORE_SERVER_IO_ERROR": "1"})

    def test_explicit_other_override(self):
        env = {"RUSTC_WRAPPER": "other", "CARGO_BUILD_RUSTC_WRAPPER": "sccache"}
        self.check_env(env, False, env)


if __name__ == "__main__":
    unittest.main(verbosity=2)
