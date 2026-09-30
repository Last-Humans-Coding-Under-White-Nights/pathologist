"""Tests for the memory-run summary (per-configuration table, digest check)."""
import json
import tempfile
import unittest
from pathlib import Path

from memory_runs_summary import collect, render, DigestMismatch


def write_run(out, label, peak_ws, peak_private, wall, user, sys_, digest):
    (out / f"{label}.json").write_text(json.dumps({
        "label": label, "exit": 0, "wall_s": wall, "user_s": user, "sys_s": sys_,
        "peak_working_set_bytes": peak_ws, "peak_private_bytes": peak_private,
    }))
    (out / f"{label}.digest").write_text(digest + "\n")


class CollectTests(unittest.TestCase):
    def test_runs_group_by_configuration_in_round_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            write_run(out, "default-2", 700 * 2**20, 650 * 2**20, 10.0, 30.0, 2.0, "abc")
            write_run(out, "default-1", 690 * 2**20, 640 * 2**20, 10.2, 30.5, 2.1, "abc")
            write_run(out, "trim-1", 500 * 2**20, 450 * 2**20, 10.4, 30.0, 2.5, "abc")
            configs = collect(out)
            self.assertEqual(list(configs), ["default", "trim"])
            self.assertEqual([r["label"] for r in configs["default"]], ["default-1", "default-2"])
            self.assertEqual(configs["trim"][0]["peak_working_set_bytes"], 500 * 2**20)

    def test_a_differing_digest_is_an_error_naming_the_run(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            write_run(out, "default-1", 1, 1, 1, 1, 1, "abc")
            write_run(out, "trim-1", 1, 1, 1, 1, 1, "xyz")
            with self.assertRaises(DigestMismatch) as cm:
                collect(out)
            self.assertIn("trim-1", str(cm.exception))

    def test_a_failed_run_is_an_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            (out / "default-1.json").write_text(json.dumps({"label": "default-1", "exit": 1}))
            (out / "default-1.digest").write_text("abc\n")
            with self.assertRaises(RuntimeError) as cm:
                collect(out)
            self.assertIn("default-1", str(cm.exception))


class RenderTests(unittest.TestCase):
    def test_table_has_one_row_per_configuration_with_deltas_against_default(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = Path(tmp)
            write_run(out, "default-1", 700 * 2**20, 650 * 2**20, 10.0, 30.0, 2.0, "abc")
            write_run(out, "default-2", 690 * 2**20, 640 * 2**20, 10.0, 30.0, 2.0, "abc")
            write_run(out, "trim-1", 490 * 2**20, 450 * 2**20, 10.5, 30.0, 2.5, "abc")
            write_run(out, "trim-2", 500 * 2**20, 450 * 2**20, 10.5, 30.0, 2.5, "abc")
            text = render(collect(out), baseline="default")
            rows = [l for l in text.splitlines() if l.startswith("| ")]
            self.assertTrue(rows[0].startswith("| Configuration |"))
            self.assertTrue(rows[1].startswith("| default |"))
            self.assertIn("700 / 690 MiB", rows[1])
            self.assertTrue(rows[2].startswith("| trim |"))
            self.assertIn("490 / 500 MiB (−29%)", rows[2])
            self.assertIn("10.5 / 10.5 s (+5.0%)", rows[2])
            self.assertIn("abc", text)


if __name__ == "__main__":
    unittest.main()
