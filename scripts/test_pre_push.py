#!/usr/bin/env python3
"""Ground pre-push range selection in real Git histories and the actual hook.

Only expensive gate commands (`cargo`, `just`) are stubbed. Real commits,
refs, ancestry, diffs, and the hook subprocess verify that a rebase does not
turn upstream fixtures into new disclosures, and that #1098's changed-scope
classification picks the right gate. No remote is contacted.

scripts/changed-scope.sh and scripts/spawn_inventory.py run FOR REAL (via
symlink into the fixture repo) — spawn_inventory.py resolves its own repo
root through its real file path, so it harmlessly re-scans the actual
newt-agent tree rather than the fixture. `just` stays stubbed: it never
executes a real recipe, so the fixture repo needs no justfile.
"""

import ipaddress
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[1]
HOOK = REPO_ROOT / ".githooks" / "pre-push"
CHANGED_SCOPE = REPO_ROOT / "scripts" / "changed-scope.sh"
SPAWN_INVENTORY = REPO_ROOT / "scripts" / "spawn_inventory.py"
ZERO = "0" * 40
DOCS_GATE = ["docs-check"]
CODE_GATE = ["docs-check", "fmt", "clippy", "nextest"]
# Generated synthetic addresses exercise the guard without embedding a host.
PRIVATE_FIXTURE = str(ipaddress.IPv4Network("10.0.0.0/8")[7])
PRIVATE_ADDITION = str(ipaddress.IPv4Network("10.0.0.0/8")[8])
METADATA_CONSTANT = "169.254.169.254"
CGNAT_CONSTANT = "100.64.0.0/10"
LINK_LOCAL_FIXTURE = str(ipaddress.IPv4Address(0xA9FE0007))
# The hook prepends ~/.cargo/bin. Exported Bash functions keep these stubs
# ahead of that path without changing HOME or invoking a real build. `just`
# only LOGS — the fixture repo carries no justfile for it to actually run.
# `cargo` logs its first word (fmt/clippy/test) so gate selection is
# observable without a real compile.
STUBS = r'''
just() { printf '%s\n' "$1" >> "$PRE_PUSH_TEST_LOG"; }
cargo() {
    # changed-scope.sh calls the REAL `cargo metadata` to map files to
    # crates; only the gate subcommands (fmt/clippy/test/nextest) are stubbed.
    if [ "$1" = "metadata" ]; then command cargo "$@"; return; fi
    printf '%s\n' "$1" >> "$PRE_PUSH_TEST_LOG"
}
cargo-nextest() { printf 'nextest\n' >> "$PRE_PUSH_TEST_LOG"; }
export -f just cargo cargo-nextest
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
        (self.root / "scripts").mkdir()
        (self.root / "scripts" / "changed-scope.sh").symlink_to(CHANGED_SCOPE)
        (self.root / "scripts" / "spawn_inventory.py").symlink_to(SPAWN_INVENTORY)
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
        if isinstance(contents, bytes):
            target.write_bytes(contents)
        else:
            target.write_text(contents)
        self.git("add", "--", path)
        self.git("commit", "-qm", "Synthetic test fixture")
        return self.git("rev-parse", "HEAD")

    def hook(self, local, remote, setup=""):
        self.log.unlink(missing_ok=True)
        refs = f"refs/heads/feature {local} refs/heads/feature {remote}\n"
        result = subprocess.run(
            ["bash", "-c", STUBS.replace('exec bash "$1"', setup + '\nexec bash "$1"'),
             "pre-push-test", str(HOOK)],
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
        self.assertEqual(calls, CODE_GATE)

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

    def test_public_network_constants_are_not_private_disclosures(self):
        for literal in [METADATA_CONSTANT, "::ffff:" + METADATA_CONSTANT, CGNAT_CONSTANT,
                        "10.0.0.1", "192.168.0.1", "127.0.0.1", "192.0.2.1"]:
            with self.subTest(literal=literal):
                local = self.commit("constant.rs", f'let public_constant = "{literal}";\n')
                result, calls = self.hook(local, self.base)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(calls, CODE_GATE)

    def test_noncanonical_link_local_and_cgnat_literals_stay_blocked(self):
        cgnat = ipaddress.IPv4Network(CGNAT_CONSTANT)
        for literal in [LINK_LOCAL_FIXTURE, "::ffff:" + LINK_LOCAL_FIXTURE,
                        str(cgnat[0]), str(cgnat[7]), str(cgnat[0]) + "/11",
                        str(cgnat[0]) + "/100", str(cgnat[0]) + "/010",
                        CGNAT_CONSTANT + "suffix", METADATA_CONSTANT + "/24"]:
            with self.subTest(literal=literal):
                local = self.commit("constant.rs", f'let synthetic_endpoint = "{literal}";\n')
                self.assert_blocked(local, self.base, literal)

    def test_allowed_literal_does_not_mask_private_neighbor_on_same_line(self):
        cgnat_host = str(ipaddress.IPv4Network(CGNAT_CONSTANT)[7])
        private_dns = ".".join(("fixture", "home", "lan"))
        for allowed in [METADATA_CONSTANT, "::ffff:" + METADATA_CONSTANT, CGNAT_CONSTANT,
                        "10.0.0.1", "192.168.0.1", "127.0.0.1", "192.0.2.1"]:
            for private in [PRIVATE_ADDITION, LINK_LOCAL_FIXTURE, cgnat_host, private_dns]:
                for literals in [(allowed, private), (private, allowed)]:
                    with self.subTest(literals=literals):
                        local = self.commit("constant.rs", f'let values = {literals!r};\n')
                        self.assert_blocked(local, self.base, private)

    def test_cgnat_range_exception_is_not_an_endpoint_exception(self):
        for literal in ["http://" + CGNAT_CONSTANT, "https://user@" + CGNAT_CONSTANT,
                        "::ffff:" + CGNAT_CONSTANT]:
            with self.subTest(literal=literal):
                local = self.commit("constant.rs", f'let synthetic_endpoint = "{literal}";\n')
                self.assert_blocked(local, self.base, literal)

    def test_scanner_failure_refuses_before_builds(self):
        local = self.commit("feature.rs", "fn fixture() {}\n")
        result, calls = self.hook(
            local, self.base, "python3() { return 73; }; export -f python3",
        )
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("network-leak scan failed", result.stdout)
        self.assertEqual(calls, [])

    def test_non_utf8_added_line_does_not_block_safe_push(self):
        local = self.commit("fixtures/non-utf8.fixture", b"\xff\n")
        result, calls = self.hook(
            local, self.base, "export PYTHONIOENCODING=utf-8:strict",
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, CODE_GATE)

    def test_unreadable_diff_refuses_before_builds(self):
        local = self.commit("feature.rs", "fn fixture() {}\n")
        result, calls = self.hook(local, "f" * 40)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("network-leak scan failed", result.stdout)
        self.assertEqual(calls, [])

    def test_docs_only_update_keeps_fast_path(self):
        local = self.commit("feature.md", "Safe documentation.\n")
        result, calls = self.hook(local, self.base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, DOCS_GATE)

    def test_code_update_keeps_full_gate(self):
        local = self.commit("feature.rs", "fn fixture() {}\n")
        result, calls = self.hook(local, self.base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, CODE_GATE)
        self.assertIn("changed scope: ALL", result.stdout)

    def test_missing_remote_object_cannot_select_docs_fast_path(self):
        local = self.commit("feature.md", "Safe documentation.\n")
        result, calls = self.hook(local, "f" * 40)
        # Refusal is safe too; a successful hook must execute the full gate.
        self.assertTrue(result.returncode != 0 or calls == CODE_GATE, result.stdout + result.stderr)
        self.assertNotIn("no code-affecting change", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
