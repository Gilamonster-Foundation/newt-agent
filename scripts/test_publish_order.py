"""Release regressions: omissions, renamed deps, unversioned paths and cycles."""

import unittest

from check_publish_order import (
    caret_matches,
    check,
    git_dependencies,
    registry_gaps,
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

    def test_dev_and_unpublished_crates_are_not_checked(self):
        crates = [
            package("app", [dependency("newtui", "^0.1.0", "dev", source=GIT)]),
            package("tool", [dependency("newtui", "^0.1.0", source=GIT)], []),
        ]
        self.assertEqual(git_dependencies(crates, ["app"]), [])

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
            ("=0.1.0", "0.1.0", False),  # unsupported req fails closed
        ):
            self.assertEqual(caret_matches(req, version), ok, (req, version))


if __name__ == "__main__":
    unittest.main()
