"""Regression guards for full JSON comparison and logical sample identities."""
import json
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from compare_slice_outputs import compare_slices


class SliceComparisons(unittest.TestCase):
    def test_equal_json_ignores_storage_rowid_changes(self):
        before = [("summary:A.first", "/absent.cpp", 4, 1)]
        after = [("summary:A.first", "/absent.cpp", 4, 99)]
        graph = {"nodes": [{"id": 1}], "edges": [{"expression": "value = payload"}]}
        with patch("compare_slice_outputs.starts", side_effect=[(1, before), (1, after)]), \
                patch("compare_slice_outputs.subprocess.run", return_value=SimpleNamespace(stdout=json.dumps(graph))) as run:
            result = compare_slices("old.db", "new.db", "old-trace", "new-trace")
        self.assertTrue(result["equal"])
        self.assertEqual(len(result["slices"]), 1)
        for call, binary, db in zip(run.call_args_list, ["old-trace", "new-trace"], ["old.db", "new.db"]):
            command = call.args[0]
            self.assertEqual(command[:3], [binary, "inspect", db])
            self.assertEqual(command[command.index("--name") + 1], "first")
            self.assertEqual(command[command.index("--up-depth") + 1], "4")
            self.assertEqual(command[command.index("--down-depth") + 1], "4")

    def test_changed_json_is_detected_even_when_sizes_match(self):
        sites = [("summary:A.first", "/absent.cpp", 4, 1)]
        graphs = [{"nodes": [{"id": 1}], "edges": [{"expression": text}]}
                  for text in ["old value", "new value"]]
        with patch("compare_slice_outputs.starts", side_effect=[(1, sites), (1, sites)]), \
                patch("compare_slice_outputs.subprocess.run", side_effect=[
                    SimpleNamespace(stdout=json.dumps(graph)) for graph in graphs]):
            result = compare_slices("old.db", "new.db", "trace", "trace")
        self.assertFalse(result["equal"])
        self.assertTrue(result["equal_sites"])
        self.assertFalse(result["slices"][0]["equal"])

    def test_changed_sample_is_reported_before_running_queries(self):
        prior = [("summary:A.first", "/absent.cpp", 4, 1)]
        current = [("summary:A.first", "/absent.cpp", 5, 1)]
        with patch("compare_slice_outputs.starts", side_effect=[(1, prior), (1, current)]), \
                patch("compare_slice_outputs.subprocess.run") as run:
            result = compare_slices("old.db", "new.db", "trace", "trace")
        self.assertFalse(result["equal_sites"])
        self.assertFalse(result["equal"])
        self.assertEqual(result["slices"], [])
        run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
