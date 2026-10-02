import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


WORKLOADS = (
    "hover_warm",
    "definition_warm",
    "completion_warm",
    "open_to_hover_large",
    "edit_to_hover_large",
    "hover_during_large_edit",
    "hover_during_large_open",
)
SCRIPT = Path(__file__).with_name("latency_report.py")


def measurement(rounds, binary_hash="a" * 64):
    return {
        "format_version": 1,
        "workload_digest": "d" * 64,
        "metadata": {
            "binary_sha256": binary_hash,
            "platform": "test-platform",
            "machine": "test-machine",
            "python": "3.test",
            "rounds": len(rounds),
            "samples": len(rounds[0]),
            "warmup": 0,
        },
        "workloads": {
            name: {"rounds_ns": [list(values) for values in rounds]}
            for name in WORKLOADS
        },
    }


def run_cli(root, baseline, candidate):
    baseline_path = root / "baseline.json"
    candidate_path = root / "candidate.json"
    baseline_path.write_text(json.dumps(baseline), encoding="utf-8")
    candidate_path.write_text(json.dumps(candidate), encoding="utf-8")
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            str(baseline_path),
            str(candidate_path),
            "--json-output",
            str(root / "report.json"),
            "--markdown-output",
            str(root / "report.md"),
        ],
        capture_output=True,
        text=True,
        check=False,
    )


class LatencyReportTests(unittest.TestCase):
    def test_report_uses_nearest_rank_and_median_of_round_percentiles(self):
        rounds = [
            [1_000_000, 2_000_000, 3_000_000, 100_000_000],
            [10_000_000, 20_000_000, 30_000_000, 40_000_000],
            [4_000_000, 5_000_000, 6_000_000, 7_000_000],
            [50_000_000, 60_000_000, 70_000_000, 80_000_000],
        ]
        candidate = [[value * 2 for value in values] for values in rounds]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = run_cli(root, measurement(rounds), measurement(candidate, "b" * 64))
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads((root / "report.json").read_text(encoding="utf-8"))
            workload = report["workloads"]["hover_warm"]
            self.assertEqual(
                workload["baseline"],
                {
                    "rounds": [
                        {"p50_ns": 2_000_000, "p95_ns": 100_000_000},
                        {"p50_ns": 20_000_000, "p95_ns": 40_000_000},
                        {"p50_ns": 5_000_000, "p95_ns": 7_000_000},
                        {"p50_ns": 60_000_000, "p95_ns": 80_000_000},
                    ],
                    "p50_ns": 12_500_000,
                    "p95_ns": 60_000_000,
                },
            )
            self.assertEqual(workload["candidate"]["p50_ns"], 25_000_000)
            self.assertEqual(workload["candidate"]["p95_ns"], 120_000_000)
            self.assertEqual(
                workload["delta"],
                {
                    "p50_ns": 12_500_000,
                    "p95_ns": 60_000_000,
                    "p50_percent": 100.0,
                    "p95_percent": 100.0,
                    "rounds": [
                        {"p50_ns": 2_000_000, "p95_ns": 100_000_000, "p50_percent": 100.0, "p95_percent": 100.0},
                        {"p50_ns": 20_000_000, "p95_ns": 40_000_000, "p50_percent": 100.0, "p95_percent": 100.0},
                        {"p50_ns": 5_000_000, "p95_ns": 7_000_000, "p50_percent": 100.0, "p95_percent": 100.0},
                        {"p50_ns": 60_000_000, "p95_ns": 80_000_000, "p50_percent": 100.0, "p95_percent": 100.0},
                    ],
                },
            )
            markdown = (root / "report.md").read_text(encoding="utf-8")
            self.assertIn("| hover_warm | 12.500 | 25.000 | +100.00% | 60.000 | 120.000 | +100.00% |", markdown)
            self.assertIn("report-only", markdown.lower())

    def test_improvement_and_even_round_half_nanosecond_median(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = run_cli(root, measurement([[3], [4]]), measurement([[1], [2]], "b" * 64))
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads((root / "report.json").read_text(encoding="utf-8"))
            workload = report["workloads"]["hover_warm"]
            self.assertEqual(workload["baseline"]["p50_ns"], 3.5)
            self.assertEqual(workload["candidate"]["p95_ns"], 1.5)
            self.assertEqual(workload["delta"]["p50_ns"], -2)
            self.assertAlmostEqual(workload["delta"]["p95_percent"], -57.14285714285714)
            self.assertIn("-57.14%", (root / "report.md").read_text(encoding="utf-8"))

    def test_descriptive_metadata_can_differ_without_affecting_pairing(self):
        baseline = measurement([[2, 4]], "a" * 64)
        candidate = measurement([[2, 4]], "b" * 64)
        baseline["metadata"]["variant_order_by_round"] = [0]
        candidate["metadata"]["variant_order_by_round"] = [1]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = run_cli(root, baseline, candidate)
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads((root / "report.json").read_text(encoding="utf-8"))
            self.assertEqual(report["workloads"]["hover_warm"]["delta"]["p95_percent"], 0)

    def test_individually_valid_but_unpaired_dimensions_fail(self):
        for candidate_rounds in ([[1, 2], [3, 4]], [[1, 2, 3]]):
            with self.subTest(rounds=candidate_rounds), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                result = run_cli(root, measurement([[1, 2]]), measurement(candidate_rounds))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("error:", result.stderr.lower())
                self.assertFalse((root / "report.json").exists())
                self.assertFalse((root / "report.md").exists())

    def test_hundredfold_regression_is_reported_without_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = run_cli(root, measurement([[1, 2, 3]]), measurement([[100, 200, 300]]))
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads((root / "report.json").read_text(encoding="utf-8"))
            for workload in report["workloads"].values():
                self.assertEqual(workload["delta"]["p50_percent"], 9900.0)
                self.assertEqual(workload["delta"]["p95_ns"], 297)

    def test_unsorted_p95_uses_nearest_rank_not_maximum_or_interpolation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            result = run_cli(
                root,
                measurement([list(range(20, 0, -1))]),
                measurement([list(range(40, 0, -2))]),
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            report = json.loads((root / "report.json").read_text(encoding="utf-8"))
            workload = report["workloads"]["hover_warm"]
            self.assertEqual(workload["baseline"]["p50_ns"], 10)
            self.assertEqual(workload["baseline"]["p95_ns"], 19)
            self.assertEqual(workload["candidate"]["p95_ns"], 38)

    def test_invalid_or_incomparable_measurements_fail_without_reports(self):
        cases = {
            "missing workload": lambda data: data["workloads"].pop("hover_warm"),
            "extra workload": lambda data: data["workloads"].update(extra={"rounds_ns": [[1, 2]]}),
            "digest mismatch": lambda data: data.update(workload_digest="e" * 64),
            "malformed digest": lambda data: data.update(workload_digest="D" * 64),
            "invalid binary hash": lambda data: data["metadata"].update(binary_sha256="bad"),
            "wrong version": lambda data: data.update(format_version=2),
            "boolean version": lambda data: data.update(format_version=True),
            "missing metadata": lambda data: data.pop("metadata"),
            "nonobject metadata": lambda data: data.update(metadata=[]),
            "nonobject workloads": lambda data: data.update(workloads=[]),
            "missing samples": lambda data: data["metadata"].pop("samples"),
            "zero rounds": lambda data: data["metadata"].update(rounds=0),
            "boolean samples": lambda data: data["metadata"].update(samples=True),
            "negative warmup": lambda data: data["metadata"].update(warmup=-1),
            "boolean warmup": lambda data: data["metadata"].update(warmup=False),
            "round count": lambda data: data["metadata"].update(rounds=2),
            "sample count": lambda data: data["metadata"].update(samples=3),
            "empty rounds": lambda data: data["workloads"]["hover_warm"].update(rounds_ns=[]),
            "empty samples": lambda data: data["workloads"]["hover_warm"].update(rounds_ns=[[]]),
            "missing measurements": lambda data: data["workloads"]["hover_warm"].clear(),
            "nonobject workload": lambda data: data["workloads"].update(hover_warm=[]),
            "boolean measurement": lambda data: data["workloads"]["hover_warm"].update(rounds_ns=[[True, 2]]),
            "zero measurement": lambda data: data["workloads"]["hover_warm"].update(rounds_ns=[[0, 2]]),
            "negative measurement": lambda data: data["workloads"]["hover_warm"].update(rounds_ns=[[-1, 2]]),
            "float measurement": lambda data: data["workloads"]["hover_warm"].update(rounds_ns=[[1.5, 2]]),
            "string measurement": lambda data: data["workloads"]["hover_warm"].update(rounds_ns=[["1", 2]]),
        }
        for field in ("platform", "machine", "python", "warmup"):
            cases[f"{field} mismatch"] = (
                lambda data, field=field: data["metadata"].update(
                    {field: 1 if field == "warmup" else "other"}
                )
            )
        for name, mutate in cases.items():
            for invalid_side in ("baseline", "candidate"):
                with self.subTest(case=name, side=invalid_side), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    baseline = measurement([[1, 2]])
                    candidate = measurement([[1, 2]])
                    mutate(baseline if invalid_side == "baseline" else candidate)
                    result = run_cli(root, baseline, candidate)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("error:", result.stderr.lower())
                    self.assertFalse((root / "report.json").exists())
                    self.assertFalse((root / "report.md").exists())

    def test_malformed_json_and_missing_files_fail(self):
        for invalid_side in ("baseline", "candidate"):
            for mode in ("malformed", "missing"):
                with self.subTest(side=invalid_side, mode=mode), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    result = run_cli(root, measurement([[1]]), measurement([[2]]))
                    (root / "report.json").unlink(missing_ok=True)
                    (root / "report.md").unlink(missing_ok=True)
                    path = root / f"{invalid_side}.json"
                    if mode == "malformed":
                        path.write_text("{not json", encoding="utf-8")
                    else:
                        path.unlink()
                    result = subprocess.run(result.args, capture_output=True, text=True, check=False)
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("error:", result.stderr.lower())
                    self.assertFalse((root / "report.json").exists())
                    self.assertFalse((root / "report.md").exists())


if __name__ == "__main__":
    unittest.main()
