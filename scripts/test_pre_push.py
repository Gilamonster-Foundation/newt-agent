#!/usr/bin/env python3
"""Ground pre-push range selection in real Git histories and the actual hook.

Only expensive gate commands (`cargo`, `just`) are stubbed. Real commits,
refs, ancestry, diffs, and the hook subprocess verify that a rebase does not
turn upstream fixtures into new disclosures, and that #1098's changed-scope
classification picks the right gate. No remote is contacted.

scripts/changed-scope.sh, scripts/reverse_deps.py and
scripts/spawn_inventory.py run FOR REAL (via symlink into the fixture repo) —
spawn_inventory.py resolves its own repo root through its real file path, so
it harmlessly re-scans the actual newt-agent tree rather than the fixture;
the other two call the real `cargo metadata` against a fixture cargo
workspace committed into the fixture repo (see `cargo_chain`). `just` stays
stubbed: it never executes a real recipe, so the fixture repo needs no
justfile.
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
REVERSE_DEPS = REPO_ROOT / "scripts" / "reverse_deps.py"
SPAWN_INVENTORY = REPO_ROOT / "scripts" / "spawn_inventory.py"
ZERO = "0" * 40
DOCS_GATE = ["docs-check"]
CODE_GATE = ["docs-check", "fmt", "clippy", "nextest"]
NO_NEXTEST_GATE = ["docs-check", "fmt", "clippy", "test"]
# Pin of the hook's NEXTEST_EXCLUDE list: binaries the per-push nextest run
# must leave to CI (custom-main binaries that misread nextest's discovery
# pass, and the real-`newt`-session / real-PTY integration binaries that
# time out under the hook's parallelism on a loaded dev box). The hook's
# header names each one with its reason; a change there updates this pin.
NEXTEST_CI_ONLY = [
    "newt-core::brush_build_pipeline",
    "agent-harness::writer_inheritance",
    "newt-git::native_git_commit",
    "newt-agent::lean_surface_purity",
    "newt-agent::stdout_purity",
    "newt-agent::posture_turn",
    "newt-agent::personality_turn",
    "newt-agent::migration_notices",
    "newt-agent::terminal_exit_pty",
    "newt-eval::mock_e2e",
]
# Same tier, but these two live as individual tests inside newt-tui's lib
# unit-test binary (not a standalone binary_id) alongside ~1,360 fast tests,
# so the hook excludes them by exact test() match scoped to that binary_id
# instead of excluding the whole binary.
NEXTEST_TEST_CI_ONLY = [
    "mcp_net_prompt_enter_confirms_configured_default_on_a_real_terminal",
    "a_permission_prompt_is_visible_and_survives_a_live_spinner",
]
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
# observable without a real compile, and its FULL argv (NUL-terminated,
# since the nextest filter expression spans lines) so package selection is.
STUBS = r'''
just() { printf '%s\n' "$1" >> "$PRE_PUSH_TEST_LOG"; }
cargo() {
    # changed-scope.sh calls the REAL `cargo metadata` to map files to
    # crates; only the gate subcommands (fmt/clippy/test/nextest) are stubbed.
    if [ "$1" = "metadata" ]; then command cargo "$@"; return; fi
    # `cargo nextest --version` is the hook's nextest-presence probe, not a
    # gate: answer it from PRE_PUSH_TEST_NO_NEXTEST (unset = present) so a
    # test can force the no-nextest fallback on a box that has nextest.
    if [ "$1" = "nextest" ] && [ "$2" = "--version" ]; then return "${PRE_PUSH_TEST_NO_NEXTEST:-0}"; fi
    printf '%s\n' "$1" >> "$PRE_PUSH_TEST_LOG"
    printf '%s\0' "$*" >> "$PRE_PUSH_TEST_ARGV"
}
export -f just cargo
exec bash "$1"
'''


@unittest.skipUnless(os.name == "posix", "the pre-push hook requires Bash and Unix utilities")
class PrePushTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="newt-pre-push-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.log = self.root / "gate-calls"
        self.argv_log = self.root / "gate-argv"
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.env.update(
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_AUTHOR_NAME="Synthetic fixture",
            GIT_AUTHOR_EMAIL="fixture@example.invalid",
            GIT_COMMITTER_NAME="Synthetic fixture",
            GIT_COMMITTER_EMAIL="fixture@example.invalid",
            PRE_PUSH_TEST_LOG=str(self.log),
            PRE_PUSH_TEST_ARGV=str(self.argv_log),
        )
        self.git("init", "-q", "--initial-branch=main", "--template=")
        (self.root / "scripts").mkdir()
        (self.root / "scripts" / "changed-scope.sh").symlink_to(CHANGED_SCOPE)
        (self.root / "scripts" / "reverse_deps.py").symlink_to(REVERSE_DEPS)
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
        self.argv_log.unlink(missing_ok=True)
        refs = f"refs/heads/feature {local} refs/heads/feature {remote}\n"
        result = subprocess.run(
            ["bash", "-c", STUBS.replace('exec bash "$1"', setup + '\nexec bash "$1"'),
             "pre-push-test", str(HOOK)],
            cwd=self.root, env=self.env, input=refs, text=True,
            capture_output=True, timeout=30,
        )
        calls = self.log.read_text().splitlines() if self.log.exists() else []
        self.argv = self.argv_log.read_text().split("\0")[:-1] if self.argv_log.exists() else []
        return result, calls

    def gate_argv(self, subcommand):
        """The one full `cargo <subcommand> ...` argv the hook issued."""
        matches = [line for line in self.argv if line.split(" ", 1)[0] == subcommand]
        self.assertEqual(len(matches), 1, self.argv)
        return matches[0]

    def cargo_chain(self):
        """Commit a real cargo workspace A <- B <- C (crate-b depends on
        crate-a, crate-c on crate-b) as the base the push starts from, so a
        later one-file edit is the only thing in the pushed range. Returns
        that base commit."""
        (self.root / "Cargo.toml").write_text(
            '[workspace]\nmembers = ["crate-a", "crate-b", "crate-c"]\nresolver = "2"\n'
        )
        for name, dep in (("crate-a", None), ("crate-b", "crate-a"), ("crate-c", "crate-b")):
            manifest = f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2021"\n'
            if dep:
                manifest += f'[dependencies]\n{dep} = {{ path = "../{dep}" }}\n'
            (self.root / name / "src").mkdir(parents=True)
            (self.root / name / "Cargo.toml").write_text(manifest)
            (self.root / name / "src" / "lib.rs").write_text("pub fn f() {}\n")
        self.git("add", "--", "Cargo.toml", "crate-a", "crate-b", "crate-c")
        self.git("commit", "-qm", "Synthetic cargo chain")
        base = self.git("rev-parse", "HEAD")
        self.git("update-ref", "refs/remotes/origin/main", base)
        return base

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

    def test_leaf_change_gates_every_transitive_reverse_dependent(self):
        # #1098 fix-first finding #1: a change in crate-a must reach clippy
        # for crate-b AND crate-c (the transitive closure), not just the crate
        # that owns the file. nextest's own `rdeps()` term covers the same set.
        base = self.cargo_chain()
        local = self.commit("crate-a/src/lib.rs", "pub fn f() { let _ = 1; }\n")
        result, calls = self.hook(local, base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("changed scope: crate-a", result.stdout)
        self.assertEqual(calls, CODE_GATE)
        self.assertIn("-p crate-a -p crate-b -p crate-c", self.gate_argv("clippy"))
        nextest = self.gate_argv("nextest")
        self.assertIn("rdeps(crate-a)", nextest)
        for binary in NEXTEST_CI_ONLY:
            self.assertIn(f"not binary_id({binary})", nextest)
        for test_name in NEXTEST_TEST_CI_ONLY:
            self.assertIn(
                f"not (binary_id(newt-tui) and test(=prompt_visibility_test::{test_name}))",
                nextest,
            )

    def test_no_nextest_fallback_gates_every_transitive_reverse_dependent(self):
        base = self.cargo_chain()
        local = self.commit("crate-a/src/lib.rs", "pub fn f() { let _ = 1; }\n")
        result, calls = self.hook(local, base, "export PRE_PUSH_TEST_NO_NEXTEST=101")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, NO_NEXTEST_GATE)
        self.assertIn("-p crate-a -p crate-b -p crate-c", self.gate_argv("test"))

    def test_top_of_chain_change_does_not_widen_to_its_dependencies(self):
        base = self.cargo_chain()
        local = self.commit("crate-c/src/lib.rs", "pub fn f() { let _ = 1; }\n")
        result, calls = self.hook(local, base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        clippy = self.gate_argv("clippy")
        self.assertIn("-p crate-c", clippy)
        self.assertNotIn("crate-a", clippy)
        self.assertNotIn("crate-b", clippy)

    def test_ci_only_binaries_stay_out_of_the_workspace_nextest_run(self):
        # Real-`newt`-session / real-PTY integration binaries are the
        # expensive tier (CLAUDE.md "Testing strategy"); on ALL the hook's
        # workspace-wide nextest run must still filter them out by name.
        local = self.commit("feature.rs", "fn fixture() {}\n")
        result, calls = self.hook(local, self.base)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("changed scope: ALL", result.stdout)
        nextest = self.gate_argv("nextest")
        for binary in NEXTEST_CI_ONLY:
            self.assertIn(f"not binary_id({binary})", nextest)
        for test_name in NEXTEST_TEST_CI_ONLY:
            self.assertIn(
                f"not (binary_id(newt-tui) and test(=prompt_visibility_test::{test_name}))",
                nextest,
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
