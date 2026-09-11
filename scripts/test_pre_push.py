#!/usr/bin/env python3
"""Ground pre-push range selection in real Git histories and the actual hook.

Only expensive gate commands are stubbed. Real commits, refs, ancestry, diffs,
and hook subprocesses verify that a rebase does not turn upstream fixtures into
new disclosures or change which local checks run. No remote is contacted.
"""

import ipaddress
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


HOOK = Path(__file__).resolve().parents[1] / ".githooks" / "pre-push"
ZERO = "0" * 40
FULL_GATE = ["check", "audit", "msrv", "cov-ci"]
# Generated synthetic addresses exercise the guard without embedding a host.
PRIVATE_FIXTURE = str(ipaddress.IPv4Network("10.0.0.0/8")[7])
PRIVATE_ADDITION = str(ipaddress.IPv4Network("10.0.0.0/8")[8])
# The hook prepends ~/.cargo/bin. Exported Bash functions keep these stubs ahead
# of that path without changing HOME or accidentally invoking a real build.
STUBS = r'''
just() { printf '%s\n' "$*" >> "$PRE_PUSH_TEST_LOG"; }
cargo() { test "$*" = 'llvm-cov --version'; }
rustup() { return 0; }
cargo-llvm-cov() { return 0; }
export -f just cargo rustup cargo-llvm-cov
exec bash "$1"
'''


@unittest.skipUnless(os.name == "posix", "the pre-push hook requires Bash and Unix utilities")
class PrePushTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="newt-pre-push-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "gate-calls"
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.env.update(
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_AUTHOR_NAME="Synthetic fixture",
            GIT_AUTHOR_EMAIL="fixture@example.invalid",
            GIT_COMMITTER_NAME="Synthetic fixture",
            GIT_COMMITTER_EMAIL="fixture@example.invalid",
            PRE_PUSH_TEST_LOG=str(self.log),
        )
        self.git("init", "-q", "--initial-branch=main", "--template=")
        self.base = self.commit("README.md", "Fixture repository.\n")
        self.git("update-ref", "refs/remotes/origin/main", self.base)

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
        self.git("commit", "-qm", "Synthetic test fixture")
        return self.git("rev-parse", "HEAD")

    def hook(self, local, remote):
        refs = f"refs/heads/feature {local} refs/heads/feature {remote}\n"
        result = subprocess.run(
            ["bash", "-c", STUBS, "pre-push-test", str(HOOK)],
            cwd=self.root, env=self.env, input=refs, text=True,
            capture_output=True, timeout=30,
        )
        calls = self.log.read_text().splitlines() if self.log.exists() else []
        return result, calls

    def rebased(self, addition=None):
        self.git("checkout", "-qb", "old-feature")
        old = self.commit("feature.md", "Feature documentation.\n")
        self.git("checkout", "-q", "main")
        upstream = self.commit("fixture.rs", f'let synthetic_address = "{PRIVATE_FIXTURE}";\n')
        self.git("update-ref", "refs/remotes/origin/main", upstream)
        self.git("checkout", "-qb", "rebased-feature")
        local = self.commit("feature.md", "Feature documentation.\n")
        if addition:
            local = self.commit("addition.rs", f'let synthetic_address = "{addition}";\n')
        return old, local

    def assert_blocked(self, local, remote, address):
        result, calls = self.hook(local, remote)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("NETWORK-LEAK GUARD", result.stdout)
        self.assertIn(address, result.stdout)
        self.assertEqual(calls, [], "privacy refusal must precede all build gates")

    def test_rebased_upstream_fixture_is_not_a_new_disclosure(self):
        old, local = self.rebased()
        result, calls = self.hook(local, old)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("NETWORK-LEAK GUARD", result.stdout)
        # The actual pushed diff includes upstream Rust even though the feature
        # itself only changes Markdown; rebasing must not select the docs gate.
        self.assertEqual(calls, FULL_GATE)
        self.assertNotIn("documentation-only push", result.stdout)

    def test_rebased_private_addition_still_fails(self):
        old, local = self.rebased(PRIVATE_ADDITION)
        self.assert_blocked(local, old, PRIVATE_ADDITION)

    def test_copying_an_upstream_address_into_a_new_file_still_fails(self):
        old, local = self.rebased(PRIVATE_FIXTURE)
        self.assert_blocked(local, old, PRIVATE_FIXTURE)

    def test_divergent_rewrite_without_current_main_still_fails(self):
        old, _ = self.rebased()
        self.git("checkout", "-qb", "divergent-feature", self.base)
        local = self.commit("addition.rs", PRIVATE_ADDITION + "\n")
        self.assert_blocked(local, old, PRIVATE_ADDITION)

    def test_fast_forward_private_addition_still_fails(self):
        local = self.commit("addition.rs", PRIVATE_ADDITION + "\n")
        self.assert_blocked(local, self.base, PRIVATE_ADDITION)

    def test_new_branch_private_addition_still_fails(self):
        local = self.commit("addition.rs", PRIVATE_ADDITION + "\n")
        self.assert_blocked(local, ZERO, PRIVATE_ADDITION)

    def test_docs_only_update_keeps_fast_path(self):
        local = self.commit("feature.md", "Safe documentation.\n")
        result, calls = self.hook(local, self.base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, ["docs-check"])

    def test_code_update_keeps_full_gate(self):
        local = self.commit("feature.rs", "fn fixture() {}\n")
        result, calls = self.hook(local, self.base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, FULL_GATE)

    def test_missing_remote_object_cannot_select_docs_fast_path(self):
        local = self.commit("feature.md", "Safe documentation.\n")
        result, calls = self.hook(local, "f" * 40)
        # Refusal is safe too; a successful hook must execute the full gate.
        self.assertTrue(result.returncode != 0 or calls == FULL_GATE, result.stdout + result.stderr)
        self.assertNotIn("documentation-only push", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
