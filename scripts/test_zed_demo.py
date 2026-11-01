#!/usr/bin/env python3
"""Consumer-visible safety contracts for the editor-demo command."""

import hashlib
import json
from pathlib import Path
import sys
import tempfile
import unittest

import zed_demo


class WorkspaceSafetyTests(unittest.TestCase):
    def test_prepare_keeps_an_existing_project_untouched(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary) / "existing project"
            run.mkdir()
            source = run / "main.bend"
            source.write_text("my unsaved demonstration must not be replaced\n")

            with self.assertRaises(FileExistsError):
                zed_demo.prepare_run(
                    run,
                    lsp=sys.executable,
                    lint=sys.executable,
                    lint_args=[],
                    compiler=sys.executable,
                )

            self.assertEqual(
                source.read_text(),
                "my unsaved demonstration must not be replaced\n",
            )
            self.assertEqual(set(run.iterdir()), {source})

    def test_changed_rule_config_invalidates_an_unchanged_source_baseline(self):
        with tempfile.TemporaryDirectory() as temporary:
            run = Path(temporary)
            source = run / "main.bend"
            source.write_bytes(b"import Base\n")
            config = run / "bend-lint.json"
            config.write_bytes(b'{"rules":{}}\n')
            baseline = {
                "schema": "bend2-zed-demo/baselines/1",
                "files": [
                    {
                        "path": path.name,
                        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                        "bytes": path.stat().st_size,
                    }
                    for path in (source, config)
                ],
            }
            (run / "baselines").mkdir()
            (run / "baselines" / "manifest.json").write_text(json.dumps(baseline))

            config.write_bytes(b'{"rules":{"a-different-rule":{}}}\n')
            with self.assertRaises(ValueError):
                zed_demo.verify_baselines(run)
            self.assertEqual(source.read_bytes(), b"import Base\n")

    def test_successful_lint_does_not_hide_a_broken_compiler_in_the_demo(self):
        report = {
            "scene": "clip3-dangerous-action",
            "invariants": {
                "disk_files_unchanged": True,
                "save_commands_invoked": False,
                "diagnostics_cleared": True,
            },
            "frames": [{
                "state": {
                    "buffer": {
                        "diagnostics": [{
                            "source": "bend2",
                            "code": "checking",
                            "severity_number": 1,
                            "message": "ENOENT: compiler/bend2/base.bend",
                        }],
                    },
                },
            }],
        }
        with self.assertRaisesRegex(ValueError, "unexpected error diagnostic"):
            zed_demo.verify_native_report(report, "clip3-dangerous-action")


if __name__ == "__main__":
    unittest.main()
