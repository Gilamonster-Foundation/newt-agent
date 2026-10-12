"""Release regressions: omissions, renamed deps, unversioned paths and cycles."""

import unittest

from check_publish_order import check, topological_order


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


if __name__ == "__main__":
    unittest.main()
