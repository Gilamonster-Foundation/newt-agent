"""#3025: the `#[ignore]`d host-shell controlling-terminal regression must be wired
into the real-PTY workflow, or reverting `setsid` leaves every automated gate green."""

import pathlib
import re
import unittest

WORKFLOW = pathlib.Path(__file__).resolve().parent.parent / ".github/workflows/newt-tui-pty.yml"


class PtyTierWiring(unittest.TestCase):
    def setUp(self):
        self.text = WORKFLOW.read_text()

    def test_host_shell_session_tests_run_including_ignored_single_threaded(self):
        step = re.search(r"cargo test -p newt-core --lib host_timeout_tree_tests[^\n]*\\?\n[^\n]*", self.text)
        self.assertIsNotNone(step, "no step runs host_timeout_tree_tests")
        self.assertIn("--include-ignored", step.group(0))
        self.assertIn("--test-threads=1", step.group(0))

    def test_changes_to_the_spawn_or_its_test_trigger_the_tier(self):
        paths = self.text.split("schedule:")[0]
        for changed in (
            "newt-core/src/agentic/tools/shell.rs",
            "newt-core/src/agentic/tools_tests/host_timeout_tree_tests.rs",
        ):
            self.assertIn(f'"{changed}"', paths)


if __name__ == "__main__":
    unittest.main()
