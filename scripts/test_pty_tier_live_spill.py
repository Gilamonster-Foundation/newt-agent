"""#3023: the `live-spill` transcript-pager PTY tests are compiled out unless the
workflow enables the feature; a green tier must mean they ran."""

import pathlib
import re
import unittest

WORKFLOW = pathlib.Path(__file__).resolve().parent.parent / ".github/workflows/newt-tui-pty.yml"


class PtyTierWiring(unittest.TestCase):
    def setUp(self):
        self.text = WORKFLOW.read_text()

    def test_every_transcript_pager_run_enables_live_spill(self):
        runs = re.findall(r"cargo test -p newt-tui ([^\n]*transcript_pager_pty_test)", self.text)
        self.assertEqual(len(runs), 2, "expected the Linux and macOS invocations")
        for run in runs:
            self.assertIn("live-spill", run)

    def test_fold_list_changes_trigger_the_tier(self):
        self.assertIn('"newt-tui/src/fold_list.rs"', self.text.split("schedule:")[0])


if __name__ == "__main__":
    unittest.main()
