#!/usr/bin/env python3
"""Mock Cargo and sccache on PATH; exercise the actual _cargo recipe offline."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class CargoFallbackTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="newt-cargo-fallback-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.cwd = self.root / "project" / "nested"
        self.cwd.mkdir(parents=True)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.home = self.root / "cargo-home"
        self.home.mkdir()
        shutil.copy(ROOT / "justfile", self.cwd / "justfile")
        (self.cwd / "scripts").symlink_to(ROOT / "scripts", target_is_directory=True)
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(("CARGO_", "RUSTC", "SCCACHE_"))}
        self.env.update(PATH=f"{self.bin}:{os.environ['PATH']}",
                        CARGO_HOME=str(self.home), TEST_LOG=str(self.root / "log"),
                        PROBE_STATUS="1", CARGO_STATUS="0")
        self.stub("cargo", '''import json, os, sys
with open(os.environ["TEST_LOG"], "w") as f:
    json.dump({"args": sys.argv[1:], "env": dict(os.environ)}, f)
sys.exit(int(os.environ["CARGO_STATUS"]))
''')
        self.stub("sccache", '''import os, sys
with open(os.environ["TEST_LOG"] + ".probe", "w") as f:
    f.write("\\n".join(sys.argv[1:]))
sys.exit(int(os.environ["PROBE_STATUS"]))
''')

    def stub(self, name, body):
        path = self.bin / name
        path.write_text(f"#!{shutil.which('python3')}\n{body}")
        path.chmod(0o755)

    def config(self, directory, value, name="config.toml"):
        directory.mkdir(parents=True, exist_ok=True)
        (directory / name).write_text(f"[build]\nrustc-wrapper = {json.dumps(value)}\n")

    def run_cargo(self, fallback=False, probe=False, status=0, args=None):
        args = args or ["build", "--release", "--bin", "newt"]
        result = subprocess.run(["just", "--justfile", str(self.cwd / "justfile"),
                                 "_cargo", *args], cwd=self.cwd, env=self.env,
                                capture_output=True, text=True)
        self.assertEqual(result.returncode, status, result.stderr)
        logged = json.loads((self.root / "log").read_text())
        self.assertEqual(logged["args"], args)
        expected = {k: v for k, v in self.env.items()
                    if k.startswith(("RUSTC", "CARGO_BUILD_RUSTC_WRAPPER"))}
        if fallback:
            expected["RUSTC_WRAPPER"] = ""
        actual = {k: v for k, v in logged["env"].items()
                  if k.startswith(("RUSTC", "CARGO_BUILD_RUSTC_WRAPPER"))}
        self.assertEqual(actual, expected)
        if probe:
            self.assertEqual(logged["env"]["SCCACHE_IGNORE_SERVER_IO_ERROR"], "1")
        else:
            self.assertNotIn("SCCACHE_IGNORE_SERVER_IO_ERROR", logged["env"])
        self.assertEqual("sccache is unavailable; building without the cache" in result.stderr,
                         fallback)
        probe_log = self.root / "log.probe"
        self.assertEqual(probe_log.exists(), probe)
        if probe:
            self.assertEqual(probe_log.read_text(), "rustc\n-vV")
        return logged

    def test_failed_explicit_sccache(self):
        self.env["RUSTC_WRAPPER"] = "sccache"
        self.run_cargo(fallback=True, probe=True)

    def test_healthy_sccache(self):
        self.env.update(RUSTC_WRAPPER="sccache", PROBE_STATUS="0")
        self.run_cargo(probe=True)

    def test_no_wrapper(self):
        self.run_cargo()

    def test_other_wrapper(self):
        self.env["RUSTC_WRAPPER"] = "custom-wrapper"
        self.run_cargo()

    def test_build_env_sccache(self):
        self.env["CARGO_BUILD_RUSTC_WRAPPER"] = "sccache"
        self.run_cargo(fallback=True, probe=True)

    def test_config_sccache(self):
        self.config(self.cwd / ".cargo", "sccache")
        self.run_cargo(fallback=True, probe=True)

    def test_parent_config_sccache(self):
        self.config(self.cwd.parent / ".cargo", "sccache")
        self.run_cargo(fallback=True, probe=True)

    def test_home_config_sccache(self):
        self.config(self.home, "sccache")
        self.run_cargo(fallback=True, probe=True)

    def test_legacy_config_wins(self):
        self.config(self.cwd / ".cargo", "sccache", "config")
        self.config(self.cwd / ".cargo", "other")
        self.run_cargo(fallback=True, probe=True)

    def test_nearest_config_wins(self):
        self.config(self.cwd / ".cargo", "other")
        self.config(self.home, "sccache")
        self.run_cargo()

    def test_env_precedence_and_empty(self):
        self.config(self.cwd / ".cargo", "sccache")
        for values in ({"RUSTC_WRAPPER": "", "CARGO_BUILD_RUSTC_WRAPPER": "sccache"},
                       {"RUSTC_WRAPPER": "other", "CARGO_BUILD_RUSTC_WRAPPER": "sccache"},
                       {"CARGO_BUILD_RUSTC_WRAPPER": ""},
                       {"CARGO_BUILD_RUSTC_WRAPPER": "other"}):
            with self.subTest(values=values):
                self.env.pop("RUSTC_WRAPPER", None)
                self.env.update(values)
                self.run_cargo()

    def test_absolute_sccache_path(self):
        self.env["RUSTC_WRAPPER"] = str(self.bin / "sccache")
        self.run_cargo(fallback=True, probe=True)

    def test_config_relative_sccache_path(self):
        self.config(self.cwd.parent / ".cargo", "../bin/sccache")
        self.run_cargo(fallback=True, probe=True)

    def test_healthy_config_and_build_env_sccache(self):
        self.config(self.cwd / ".cargo", "sccache")
        self.env["PROBE_STATUS"] = "0"
        self.run_cargo(probe=True)
        self.env["CARGO_BUILD_RUSTC_WRAPPER"] = "sccache"
        self.run_cargo(probe=True)

    def test_empty_config_overrides_parent(self):
        self.config(self.cwd / ".cargo", "")
        self.config(self.home, "sccache")
        self.run_cargo()

    def test_wrapper_in_parent_when_nearest_has_other_settings(self):
        self.config(self.cwd / ".cargo", "sccache")
        (self.cwd / ".cargo" / "config.toml").write_text("[build]\njobs = 2\n")
        self.config(self.home, "sccache")
        self.run_cargo(fallback=True, probe=True)

    def test_arguments_with_spaces(self):
        self.run_cargo(args=["build", "--target-dir", "directory with spaces; $literal"])

    def test_cargo_failure_propagates(self):
        self.env.update(RUSTC_WRAPPER="sccache", CARGO_STATUS="42")
        self.run_cargo(fallback=True, probe=True, status=42)


if __name__ == "__main__":
    unittest.main(verbosity=2)
