"""Release regressions: omissions, renamed deps, unversioned paths and cycles."""

import io
import json
import unittest
from unittest import mock
import urllib.error

import check_publish_order
from check_publish_order import (
    PUBLISH_JOBS,
    REGISTRY_TIMEOUT,
    RELEASE_GATES,
    check,
    crates_io_versions,
    gate_errors,
    git_dependencies,
    registry_gaps,
    req_matches,
    topological_order,
)


def package(name, dependencies=(), publish=None):
    return {
        "name": name,
        "version": "0.8.0",
        "dependencies": list(dependencies),
        "publish": publish,
    }


def dependency(name, req="=0.8.0", kind=None, **extra):
    return {"name": name, "req": req, "kind": kind, **extra}


class PublishOrder(unittest.TestCase):
    def test_missing_leaf_is_not_silently_unreleased(self):
        self.assertTrue(check([package("leaf")], []))

    def test_harness_rename_uses_package_not_import_alias(self):
        crates = [
            package("newt-harness"),
            package("app", [dependency("newt-harness", rename="agent-harness")]),
        ]
        self.assertEqual(check(crates, ["newt-harness", "app"]), [])
        self.assertTrue(check(crates, ["app", "newt-harness"]))
        self.assertTrue(check(crates, ["agent-harness", "app"]))

    def test_normal_path_only_and_version_drift_fail(self):
        for req in ("*", "^0.8.0", "=0.7.0"):
            self.assertTrue(
                check(
                    [package("leaf"), package("app", [dependency("leaf", req)])],
                    ["leaf", "app"],
                )
            )

    def test_only_stripped_dev_helpers_are_exempt(self):
        crates = [
            package("app", [dependency("helper", "*", "dev")]),
            package("helper", [dependency("app")], []),
        ]
        self.assertEqual(check(crates, ["app"]), [])
        crates[0]["dependencies"][0]["req"] = "=0.8.0"
        self.assertTrue(check(crates, ["app", "helper"]))

    def test_topological_suggestion_includes_leaves_and_detects_cycles(self):
        crates = [
            package("app", [dependency("leaf")]),
            package("leaf"),
            package("extra"),
        ]
        order = topological_order(crates)
        self.assertEqual(check(crates, order), [])
        crates[1]["dependencies"] = [dependency("app")]
        with self.assertRaisesRegex(ValueError, "dependency cycle"):
            topological_order(crates)

    def test_optional_target_and_build_edges_are_not_dropped(self):
        for extra in (
            {"optional": True},
            {"target": "cfg(windows)"},
            {"kind": "build"},
        ):
            crates = [package("app", [dependency("leaf", **extra)]), package("leaf")]
            self.assertEqual(check(crates, ["leaf", "app"]), [])
            self.assertIn(
                "'app' is published before dependency 'leaf'",
                check(crates, ["app", "leaf"]),
            )

    def test_disabled_duplicate_and_unknown_entries_fail(self):
        crates = [package("app"), package("private", publish=[])]
        for matrix, message in (
            (["app", "private"], "'private' is marked publish = false"),
            (["app", "app"], "duplicate crate in publish matrix"),
            (["app", "unknown"], "'unknown' is not a workspace package"),
        ):
            self.assertIn(message, check(crates, matrix))


GIT = "git+https://github.com/Gilamonster-Foundation/newtui?rev=b7ce8ee"


class GitDependencies(unittest.TestCase):
    """v0.8.0: newt-core shipped a git-only `newtui` dep that crates.io lacked."""

    def test_git_only_dependency_missing_from_crates_io_fails(self):
        crates = [package("newt-core", [dependency("newtui", "^0.1.0", source=GIT)])]
        deps = git_dependencies(crates, ["newt-core"])
        self.assertEqual(deps, [("newt-core", "newtui", "^0.1.0")])
        self.assertTrue(registry_gaps(deps, lambda _: []))
        self.assertTrue(registry_gaps(deps, lambda _: ["0.1.0-rc.1"]))
        self.assertEqual(registry_gaps(deps, lambda _: ["0.1.0"]), [])

    def test_unversioned_dev_helpers_and_unpublished_crates_are_not_checked(self):
        """Cargo strips a dev-dependency with no version from the package."""
        crates = [
            package("app", [dependency("newtui", "*", "dev", source=GIT)]),
            package("tool", [dependency("newtui", "^0.1.0", source=GIT)], []),
        ]
        self.assertEqual(git_dependencies(crates, ["app"]), [])

    def test_versioned_dev_dependency_is_checked(self):
        """Cargo keeps a versioned dev-dependency in the published manifest, and
        `cargo package` fails when the registry lacks it (PR #2871 review P2-2)."""
        crates = [package("app", [dependency("newtui", "^0.1.0", "dev", source=GIT)])]
        deps = git_dependencies(crates, ["app"])
        self.assertEqual(deps, [("app", "newtui", "^0.1.0")])
        self.assertTrue(registry_gaps(deps, lambda _: []))

    def test_caret_semantics(self):
        for req, version, ok in (
            ("^0.1", "0.1.0", True),
            ("^0.1", "0.1.9", True),
            ("^0.1", "0.2.0", False),
            ("^0.1.2", "0.1.1", False),
            ("^1.2", "1.9.0", True),
            ("^1.2", "2.0.0", False),
            ("^0.0.3", "0.0.4", False),
            ("^0.1", "0.1.0-rc.1", False),
            ("0.1", "0.1.5", True),  # bare requirement is caret
            ("^0", "0.9.0", True),
        ):
            self.assertEqual(req_matches(req, version), ok, (req, version))

    def test_cargo_requirement_operators(self):
        """Cargo's requirement syntax, not just caret (PR #2871 review P2-1)."""
        for req, version, ok in (
            ("=0.1.0", "0.1.0", True),
            ("=0.1.0", "0.1.1", False),
            ("=0.1", "0.1.7", True),
            ("~0.1.0", "0.1.5", True),
            ("~0.1.2", "0.1.1", False),
            ("~0.1", "0.2.0", False),
            ("~1", "1.9.0", True),
            (">=0.1.0, <0.2.0", "0.1.3", True),
            (">=0.1.0, <0.2.0", "0.2.0", False),
            (">0.1.0", "0.1.0", False),
            ("<=0.1.0", "0.1.0", True),
            ("*", "3.4.5", True),
            ("0.1.*", "0.1.9", True),
            ("0.1.*", "0.2.0", False),
            ("1.x", "1.4.0", True),
        ):
            self.assertEqual(req_matches(req, version), ok, (req, version))

    def test_prereleases_match_only_when_requested(self):
        for req, version, ok in (
            ("^0.1.0-rc.1", "0.1.0-rc.1", True),
            ("^0.1.0-rc.1", "0.1.0-rc.2", True),
            ("^0.1.0-rc.1", "0.1.0", True),
            ("^0.1.0-rc.2", "0.1.0-rc.1", False),
            ("^0.1.0-rc.1", "0.1.1-rc.1", False),  # other triple's prerelease
            ("=0.1.0-alpha.10", "0.1.0-alpha.10", True),
            ("^0.1.0-alpha.2", "0.1.0-alpha.10", True),  # numeric identifiers
            ("^0.1.0-alpha", "0.1.0-alpha.1", True),
            (">=0.1.0-beta", "0.1.0-alpha", False),
            ("*", "1.0.0-rc.1", False),
        ):
            self.assertEqual(req_matches(req, version), ok, (req, version))

    def test_each_comparator_keeps_its_own_prerelease_rule(self):
        """A wildcard or partial tilde rejects a prerelease on its own; another
        comparator naming that prerelease cannot rescue it (PR #2871 r2 P2-1).
        Expected values are `semver::VersionReq::matches` 1.0.28."""
        for req, version, ok in (
            ("1.*, >=1.2.3-alpha", "1.2.3-alpha", False),
            ("~1.2, >=1.2.3-alpha", "1.2.3-alpha", False),
            ("1.2.*, >=1.2.3-alpha", "1.2.3-alpha", False),
            ("~1, >=1.2.3-alpha", "1.2.3-alpha", False),
            ("=1.2, >=1.2.3-alpha", "1.2.3-alpha", False),
            # Partial carets have no prerelease restriction of their own.
            ("^1.2, >=1.2.3-alpha", "1.2.3-alpha", True),
            ("^1, >=1.2.3-alpha", "1.2.3-alpha", True),
            ("~1.2.3-alpha", "1.2.3-alpha", True),
        ):
            self.assertEqual(req_matches(req, version), ok, (req, version))
        deps = [("app", "newtui", "1.*, >=1.2.3-alpha")]
        self.assertTrue(registry_gaps(deps, lambda _: ["1.2.3-alpha"]))

    def test_build_metadata_is_ignored(self):
        self.assertTrue(req_matches("^0.1.0", "0.1.0+build.1"))
        self.assertTrue(req_matches("=0.1.0", "0.1.0+build.1"))
        self.assertTrue(req_matches("^0.1.0-rc.1", "0.1.0-rc.1+sha.abc"))
        deps = [("newt-core", "newtui", "^0.1.0")]
        self.assertEqual(registry_gaps(deps, lambda _: ["0.1.0+build.1"]), [])

    def test_exact_pin_in_registry_is_not_a_gap(self):
        deps = [("newt-core", "newtui", "=0.1.0")]
        self.assertEqual(registry_gaps(deps, lambda _: ["0.1.0"]), [])
        self.assertTrue(registry_gaps(deps, lambda _: ["0.1.1"]))

    def test_unparseable_requirement_or_index_version_fails_closed(self):
        deps = [("newt-core", "newtui", "^not.a.version")]
        self.assertIn("cannot parse", registry_gaps(deps, lambda _: ["0.1.0"])[0])
        ok = [("newt-core", "newtui", "^0.1.0")]
        self.assertEqual(registry_gaps(ok, lambda _: ["garbage", "0.1.0"]), [])
        self.assertTrue(registry_gaps(ok, lambda _: ["garbage"]))

class RegistryLookup(unittest.TestCase):
    """The network half of the preflight is bounded and fails closed (P2-3)."""

    def opener(self, body=b"", error=None):
        calls = []

        def urlopen(url, timeout=None):
            calls.append((url, timeout))
            if error:
                raise error
            return io.BytesIO(body)

        return urlopen, calls

    def test_requests_carry_a_finite_timeout(self):
        body = b'{"vers":"0.1.0"}\n{"vers":"0.1.1","yanked":true}\n'
        urlopen, calls = self.opener(body)
        self.assertEqual(crates_io_versions("newtui", urlopen), ["0.1.0"])
        self.assertEqual(calls, [("https://index.crates.io/ne/wt/newtui", REGISTRY_TIMEOUT)])
        self.assertTrue(0 < REGISTRY_TIMEOUT < 120)

    def test_missing_crate_is_an_empty_release_list(self):
        error = urllib.error.HTTPError("u", 404, "Not Found", None, None)
        urlopen, _ = self.opener(error=error)
        self.assertEqual(crates_io_versions("newtui", urlopen), [])

    def test_fetch_errors_fail_closed_with_a_gap(self):
        deps = [("newt-core", "newtui", "^0.1.0")]
        for error in (
            urllib.error.HTTPError("u", 503, "Unavailable", None, None),
            urllib.error.URLError(TimeoutError("timed out")),
            TimeoutError("timed out"),
        ):
            urlopen, _ = self.opener(error=error)
            gaps = registry_gaps(deps, lambda n, o=urlopen: crates_io_versions(n, o))
            self.assertEqual(len(gaps), 1, error)
            self.assertIn("could not query crates.io", gaps[0])

    def test_malformed_index_fails_closed(self):
        urlopen, _ = self.opener(b"not json\n")
        deps = [("newt-core", "newtui", "^0.1.0")]
        gaps = registry_gaps(deps, lambda n: crates_io_versions(n, urlopen))
        self.assertIn("could not query crates.io", gaps[0])

    def test_repeated_lookups_for_a_crate_are_cached_within_a_run(self):
        seen = []
        deps = [
            ("newt-core", "newtui", "^0.1.0"),
            ("newt-tui", "newtui", "^0.1.0"),
            ("newt-tui", "precedence-ladder", "^0.1"),
        ]
        registry_gaps(deps, lambda n: seen.append(n) or ["0.1.0"])
        self.assertEqual(seen, ["newtui", "precedence-ladder"])


class PublishGates(unittest.TestCase):
    """#2873: v0.8.0 uploaded crates and npm packages before a gate went red."""

    def jobs(self):
        jobs = {gate: {} for gate in RELEASE_GATES}
        jobs["mcp-import-acceptance"] = {"needs": ["setup-acceptance"]}
        jobs["build-binaries"] = {"needs": "mcp-import-acceptance"}
        jobs.update({job: {"needs": list(RELEASE_GATES)} for job in PUBLISH_JOBS})
        return jobs

    def test_every_publish_job_waiting_on_every_gate_passes(self):
        self.assertEqual(gate_errors(self.jobs()), [])

    def test_transitive_need_counts(self):
        jobs = self.jobs()
        jobs["publish-npm-meta"] = {"needs": ["build-npm"]}
        self.assertEqual(gate_errors(jobs), [])

    def test_publish_racing_a_preflight_fails(self):
        jobs = self.jobs()
        jobs["publish-crates"]["needs"] = ["build-binaries", "check-publish-order"]
        errors = gate_errors(jobs)
        self.assertIn("'publish-crates' can publish before gate 'npm-preflight' passes", errors)
        self.assertNotIn("'publish-crates' can publish before gate 'setup-acceptance' passes", errors)

    def test_missing_publish_job_fails(self):
        jobs = self.jobs()
        del jobs["publish-pypi"]
        self.assertIn("publish job 'publish-pypi' is missing from release.yml", gate_errors(jobs))



class CombinedMain(unittest.TestCase):
    """#2871 + #2891: main() must run BOTH the gate check and the registry
    preflight. A merge that keeps either side's `errors =` hunk alone stays
    green in every other test class and silently drops one gate."""

    def run_main(self, jobs, versions):
        metadata = {
            "packages": [
                package("newt-core", [dependency("newtui", "^0.1.0", source=GIT)])
            ]
        }
        workflow = {"jobs": jobs}
        workflow["jobs"]["publish-crates"]["strategy"] = {
            "matrix": {"crate": ["newt-core"]}
        }
        fake_path = mock.Mock(return_value=mock.Mock(read_text=lambda: ""))
        out = io.StringIO()
        with mock.patch.object(check_publish_order, "Path", fake_path), \
                mock.patch.object(check_publish_order.yaml, "safe_load", return_value=workflow), \
                mock.patch.object(check_publish_order, "crates_io_versions", lambda _: versions), \
                mock.patch("sys.argv", ["check_publish_order.py", "-"]), \
                mock.patch("sys.stdin", io.StringIO(json.dumps(metadata))), \
                mock.patch("sys.stdout", out):
            code = check_publish_order.main()
        return code, out.getvalue()

    def test_main_reports_both_gate_and_registry_errors(self):
        jobs = PublishGates().jobs()
        jobs["publish-crates"]["needs"] = ["check-publish-order"]
        code, out = self.run_main(jobs, [])
        self.assertEqual(code, 1)
        self.assertIn("'publish-crates' can publish before gate 'npm-preflight' passes", out)
        self.assertIn("crates.io has no release matching it", out)

    def test_main_passes_when_both_hold(self):
        code, out = self.run_main(PublishGates().jobs(), ["0.1.0"])
        self.assertEqual(code, 0, out)


if __name__ == "__main__":
    unittest.main()
