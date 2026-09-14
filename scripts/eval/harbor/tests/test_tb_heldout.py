"""The held-out pool excludes every tuning-exposed task, and the draw is
reproducible and keeps each declared-difficulty stratum's share.
Run: PYTHONPATH=scripts/eval/harbor python3 -m unittest discover -s scripts/eval/harbor/tests -p 'test_tb_heldout.py'
"""

import json
import unittest
from collections import Counter
from pathlib import Path

from tb_heldout import pool, stratified_draw, task_names

HARBOR = Path(__file__).resolve().parent.parent


def tasks(n_medium, n_hard):
    return [{"task": f"m{i:02}", "difficulty": "medium"} for i in range(n_medium)] + \
           [{"task": f"h{i:02}", "difficulty": "hard"} for i in range(n_hard)]


class HeldOut(unittest.TestCase):
    def test_pool_drops_exposed_tasks(self):
        meta = {"a": {"difficulty": "easy"}, "b": {"difficulty": "hard"}, "c": {"difficulty": "medium"}}
        self.assertEqual([t["task"] for t in pool(meta, {"a"})], ["b", "c"])

    def test_committed_pool_is_disjoint_from_every_committed_tuning_set(self):
        committed = {t["task"] for t in json.loads((HARBOR / "tb-heldout-pool.json").read_text())}
        for exposed in ("tb-30.json", "smart-ab-12.json", "smart-ab-8.json"):
            self.assertFalse(committed & set(task_names(HARBOR / exposed)), exposed)

    def test_draw_is_seeded_and_stratified_by_largest_remainder(self):
        drawn = stratified_draw(tasks(33, 19), 24, 2318)
        self.assertEqual(Counter(t["difficulty"] for t in drawn), {"medium": 15, "hard": 9})
        self.assertEqual(drawn, stratified_draw(tasks(33, 19), 24, 2318))
        self.assertNotEqual(drawn, stratified_draw(tasks(33, 19), 24, 1))

    def test_cannot_draw_more_than_the_pool(self):
        with self.assertRaises(ValueError):
            stratified_draw(tasks(2, 1), 4, 0)


if __name__ == "__main__":
    unittest.main()
