#!/usr/bin/env python3
"""Ground changed-scope.sh (issue #1098) in a real git history and a real
fake cargo workspace, so the crate-mapping rule is proven against `cargo
metadata`, not a stub of it.
"""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent / "changed-scope.sh"
ZERO = "0" * 40


@unittest.skipUnless(os.name == "posix", "changed-scope.sh requires Bash and Unix utilities")
class ChangedScopeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="newt-changed-scope-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.env.update(
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_AUTHOR_NAME="Synthetic fixture",
            GIT_AUTHOR_EMAIL="fixture@example.invalid",
            GIT_COMMITTER_NAME="Synthetic fixture",
            GIT_COMMITTER_EMAIL="fixture@example.invalid",
        )
        self._write_fake_workspace()
        self.git("init", "-q", "--initial-branch=main", "--template=")
        self.git("add", "-A")
        self.git("commit", "-qm", "fixture workspace")
        self.base = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", self.base)

    def _write_fake_workspace(self):
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crate-a", "crate-b"]\nresolver = "2"\n'
        )
        for name in ("crate-a", "crate-b"):
            crate_dir = self.root / name
            (crate_dir / "src").mkdir(parents=True)
            (crate_dir / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
            )
            (crate_dir / "src" / "lib.rs").write_text("pub fn f() {}\n")
        (self.root / "Cargo.lock").write_text("# fixture lock\n")
        (self.root / "README.md").write_text("Fixture workspace.\n")
        (self.root / "docs").mkdir()
        (self.root / "docs" / "notes.md").write_text("Fixture notes.\n")

    def git(self, *args):
        result = subprocess.run(
            ["git", *args], cwd=self.root, env=self.env,
            text=True, capture_output=True, check=True,
        )
        return result.stdout.strip()

    def commit(self, path, contents):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(contents)
        self.git("add", "--", path)
        self.git("commit", "-qm", "fixture change")
        return self.git("rev-parse", "HEAD")

    def run_script(self, local, remote, local_ref="refs/heads/feature"):
        refs = f"{local_ref} {local} {local_ref} {remote}\n"
        result = subprocess.run(
            ["bash", str(SCRIPT)], cwd=self.root, env=self.env,
            input=refs, text=True, capture_output=True, timeout=30,
        )
        return result

    def test_tag_push_skips(self):
        local = self.commit("crate-a/src/lib.rs", "pub fn f() { 1; }\n")
        result = self.run_script(local, ZERO, local_ref="refs/tags/v1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "")

    def test_deletion_skips(self):
        local = self.commit("crate-a/src/lib.rs", "pub fn f() { 1; }\n")
        result = self.run_script(ZERO, local)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "")

    def test_docs_only_skips(self):
        local = self.commit("docs/notes.md", "More fixture notes.\n")
        result = self.run_script(local, self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "")

    def test_crate_a_change_selects_crate_a(self):
        local = self.commit("crate-a/src/lib.rs", "pub fn f() { 2; }\n")
        result = self.run_script(local, self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.split(), ["crate-a"])

    def test_lockfile_change_selects_all(self):
        local = self.commit("Cargo.lock", "# fixture lock v2\n")
        result = self.run_script(local, self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "ALL")

    def test_new_branch_uses_merge_base(self):
        # Diverge crate-a on main after the branch forked, so the branch's
        # OWN new commit (crate-b) is the only thing a merge-base diff sees.
        self.git("checkout", "-qb", "feature", self.base)
        local = self.commit("crate-b/src/lib.rs", "pub fn f() { 3; }\n")
        self.git("checkout", "-q", "main")
        upstream = self.commit("crate-a/src/lib.rs", "pub fn f() { 4; }\n")
        self.git("update-ref", "refs/remotes/origin/main", upstream)

        result = self.run_script(local, ZERO)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.split(), ["crate-b"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
