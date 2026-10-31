"""GNU time capability checks run before any merger or input is opened."""
import contextlib
import io
import subprocess
import unittest
from unittest.mock import patch

import bench_merge


class TimerCheck(unittest.TestCase):
    def rejects_timer(self, timer, result=None, error=None):
        args = ["bench_merge.py", "--baseline", "missing-baseline", "--candidate",
                "missing-candidate", "--outdir", "unused", "missing.db"]
        stderr = io.StringIO()
        with patch("sys.argv", args), patch.object(bench_merge.shutil, "which", return_value=timer), \
                patch.object(bench_merge.subprocess, "run", return_value=result, side_effect=error) as run, \
                patch.object(bench_merge, "sha256") as digest, contextlib.redirect_stderr(stderr):
            with self.assertRaises(SystemExit) as raised:
                bench_merge.main()
            self.assertEqual(raised.exception.code, 2)
            self.assertIn("GNU time is required", stderr.getvalue())
            digest.assert_not_called()
            self.assertEqual(run.call_count, int(timer is not None))
            if timer:
                self.assertEqual(run.call_args.args[0], [timer, "-f", "%M", "true"])

    def test_missing_timer(self):
        self.rejects_timer(None)

    def test_bsd_timer(self):
        self.rejects_timer("/usr/bin/time", result=subprocess.CompletedProcess([], 1))

    def test_unusable_timer(self):
        self.rejects_timer("/usr/bin/time", error=OSError("cannot execute"))

    def test_timer_timeout(self):
        self.rejects_timer("/usr/bin/time", error=subprocess.TimeoutExpired("time", 5))


if __name__ == "__main__":
    unittest.main()
