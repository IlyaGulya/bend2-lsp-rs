#!/usr/bin/env python3
"""Exercise the shared pinned-tool version check through its shell interface."""

from pathlib import Path
import subprocess
import unittest


HELPER = Path(__file__).resolve().with_name("check-tool-version.sh")


class ToolVersionTests(unittest.TestCase):
    def check_version(self, reported):
        return subprocess.run(
            [
                "bash", "-c",
                'source "$1"; check_tool_version audit-tool 0.20.2 "$2"',
                "audit", str(HELPER), reported,
            ],
            capture_output=True, text=True, check=False,
        )

    def test_exact_version_formats(self):
        for reported in ("0.20.2", "cargo-deny 0.20.2", "0.20.2\nbuild details"):
            with self.subTest(reported=reported):
                self.assertEqual(self.check_version(reported).returncode, 0)

    def test_wrong_or_unavailable_versions_fail(self):
        for reported in (
            "cargo-deny 0.20.20", "cargo-deny 0.20.2-beta", "cargo-deny 0.20.1",
            "unknown", "cargo-deny\n0.20.2",
        ):
            with self.subTest(reported=reported):
                result = self.check_version(reported)
                self.assertEqual(result.returncode, 1)
                self.assertIn("Expected audit-tool version 0.20.2", result.stderr)


if __name__ == "__main__":
    unittest.main()
