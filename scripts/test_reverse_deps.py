#!/usr/bin/env python3
"""Ground reverse_deps.py (issue #1098 fix-first finding #1) in a real
3-crate cargo workspace: crate-b depends on crate-a, crate-c is independent.
"""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import reverse_deps  # noqa: E402


class ReverseDepsTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="newt-reverse-deps-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crate-a", "crate-b", "crate-c"]\nresolver = "2"\n'
        )
        (self.root / "crate-a" / "src").mkdir(parents=True)
        (self.root / "crate-a" / "Cargo.toml").write_text(
            '[package]\nname = "crate-a"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (self.root / "crate-a" / "src" / "lib.rs").write_text("pub fn f() {}\n")

        (self.root / "crate-b" / "src").mkdir(parents=True)
        (self.root / "crate-b" / "Cargo.toml").write_text(
            '[package]\nname = "crate-b"\nversion = "0.1.0"\nedition = "2021"\n'
            '[dependencies]\ncrate-a = { path = "../crate-a" }\n'
        )
        (self.root / "crate-b" / "src" / "lib.rs").write_text("pub fn g() { crate_a::f() }\n")

        (self.root / "crate-c" / "src").mkdir(parents=True)
        (self.root / "crate-c" / "Cargo.toml").write_text(
            '[package]\nname = "crate-c"\nversion = "0.1.0"\nedition = "2021"\n'
        )
        (self.root / "crate-c" / "src" / "lib.rs").write_text("pub fn h() {}\n")

        self.metadata = json.loads(
            subprocess.run(
                ["cargo", "metadata", "--format-version", "1"],
                cwd=self.root, check=True, capture_output=True, text=True,
            ).stdout
        )

    def test_leaf_change_pulls_in_its_dependent(self):
        # This is the exact regression named in the fix-first review: a
        # change to a leaf crate (crate-a) must select crate-b too, because
        # crate-b's own tests exercise crate-a's code.
        result = reverse_deps.transitive_reverse_dependents(["crate-a"], self.metadata)
        self.assertEqual(result, ["crate-a", "crate-b"])

    def test_independent_crate_stays_alone(self):
        result = reverse_deps.transitive_reverse_dependents(["crate-c"], self.metadata)
        self.assertEqual(result, ["crate-c"])

    def test_top_of_chain_has_no_dependents(self):
        result = reverse_deps.transitive_reverse_dependents(["crate-b"], self.metadata)
        self.assertEqual(result, ["crate-b"])

    def test_cli_prints_sorted_space_separated_closure(self):
        result = subprocess.run(
            [sys.executable, str(Path(__file__).resolve().parent / "reverse_deps.py"), "crate-a"],
            cwd=self.root, check=True, capture_output=True, text=True,
        )
        self.assertEqual(result.stdout.strip(), "crate-a crate-b")

    def test_no_crates_prints_nothing(self):
        result = subprocess.run(
            [sys.executable, str(Path(__file__).resolve().parent / "reverse_deps.py")],
            cwd=self.root, check=True, capture_output=True, text=True,
        )
        self.assertEqual(result.stdout, "")


if __name__ == "__main__":
    unittest.main(verbosity=2)
