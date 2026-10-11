"""Name/kind extraction must still pass the production shrink and commit gates."""

import unittest

import test_refactor as fixtures


class ExtractionIdentity(unittest.TestCase):
    def setUp(self):
        self.case = fixtures.Grade()
        self.case.setUp()
        self.addCleanup(self.case.doCleanups)
        self.case.fake.trees[fixtures.HEAD][
            "crate/src/extracted.rs"
        ] = "pub fn moved(x: different::Type) -> bool { new::work(x) }\n"

    def test_edited_signature_and_body_pass_real_helper_and_grade(self):
        """The old signature/body comparison rejected genuine structural moves."""
        report = self.case.grade()
        self.assertEqual(report["criteria"]["extraction"]["status"], "PASS", report)
        self.assertEqual(
            report["criteria"]["worktree_commit"]["status"], "PASS", report
        )

    def test_largest_file_must_still_shrink(self):
        for padding in (9, 10):
            self.case.fake.trees[fixtures.HEAD][fixtures.TARGET] = (
                "mod extracted;\n" + "// padding\n" * padding
            )
            report = self.case.grade()
            self.assertEqual(report["criteria"]["extraction"]["status"], "FAIL")
            self.assertEqual(report["criteria"]["worktree_commit"]["status"], "FAIL")

    def test_forwarding_function_does_not_count_as_removed(self):
        self.case.fake.trees[fixtures.HEAD][
            fixtures.TARGET
        ] = "mod extracted;\nfn moved() { extracted::moved() }\n"
        report = self.case.grade()
        self.assertEqual(report["criteria"]["extraction"]["status"], "FAIL")
        self.assertEqual(report["criteria"]["worktree_commit"]["status"], "FAIL")

    def test_root_or_merge_commit_does_not_prove_worktree_extraction(self):
        for parents in (
            fixtures.HEAD,
            fixtures.HEAD + " " + fixtures.SEED + " " + "c" * 40,
        ):

            def command(argv, cwd):
                if argv[:3] == ["git", "rev-list", "--parents"]:
                    return parents + "\n"
                return self.case.fake(argv, cwd)

            report = fixtures.grader.grade(self.case.args, command)
            self.assertEqual(report["criteria"]["extraction"]["status"], "PASS")
            self.assertEqual(report["criteria"]["worktree_commit"]["status"], "FAIL")
