import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from performance_policy import compare_summary, main, within_limit


METRICS = ("Ir", "I1mr", "ILmr")


def summary_data(
    baseline_name="selected",
    candidate=(100, 2, 2),
    baseline=(100, 2, 2),
    function_name="synthetic_workload",
    benchmark_id=None,
):
    """Build an Iai 0.16.1 summary: Both is [candidate, baseline]."""
    callgrind = {
        metric: {
            "metrics": {
                "Both": [
                    {"Int": candidate[index]},
                    {"Int": baseline[index]},
                ]
            }
        }
        for index, metric in enumerate(METRICS)
    }
    summary = {
        "function_name": function_name,
        "baselines": [None, baseline_name],
        "profiles": [
            {
                "summaries": {
                    "parts": [
                        {
                            "metrics_summary": {
                                "Callgrind": callgrind,
                            }
                        }
                    ]
                }
            }
        ],
    }
    if benchmark_id is not None:
        summary["id"] = benchmark_id
    return summary


def write_summary(root, filename, data):
    path = root / filename / "summary.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data), encoding="utf-8")
    return path


def run_cli(root, *arguments):
    stdout = io.StringIO()
    stderr = io.StringIO()
    with (
        patch("sys.argv", ["performance_policy.py", str(root), *arguments]),
        contextlib.redirect_stdout(stdout),
        contextlib.redirect_stderr(stderr),
    ):
        try:
            status = main()
        except SystemExit as error:
            status = error.code
    return status, stdout.getvalue(), stderr.getvalue()

def left_only(summary):
    callgrind = summary["profiles"][0]["summaries"]["parts"][0][
        "metrics_summary"
    ]["Callgrind"]
    for metric in METRICS:
        current = callgrind[metric]["metrics"]["Both"][0]
        callgrind[metric]["metrics"] = {"Left": current}
    return summary


def workload_summary(index, **kwargs):
    return summary_data(
        function_name=f"analysis::workload_{index // 3:02d}",
        benchmark_id=f"case_{index:03d}",
        **kwargs,
    )


def build_baseline_manifest(root, manifest_path, count):
    for index in range(count):
        write_summary(
            root,
            f"baseline-{index:03d}",
            left_only(workload_summary(index, baseline_name="main")),
        )
    return run_cli(root, f"--write-baseline-manifest={manifest_path}")


def write_candidate_workloads(root, candidate_count, baseline_count, regression=None):
    for index in range(candidate_count):
        candidate = (100, 2, 2)
        if index == regression:
            candidate = (103, 2, 2)
        summary = workload_summary(index, candidate=candidate)
        if index >= baseline_count:
            summary = left_only(summary)
        write_summary(root, f"candidate-{index:03d}", summary)



class RegressionLimitTests(unittest.TestCase):
    def test_two_to_three_cache_events_pass(self):
        for metric in ("I1mr", "ILmr"):
            with self.subTest(metric=metric):
                self.assertTrue(within_limit(metric, 2, 3))

    def test_two_to_six_cache_events_fail(self):
        for metric in ("I1mr", "ILmr"):
            with self.subTest(metric=metric):
                self.assertFalse(within_limit(metric, 2, 6))

    def test_sixty_two_to_sixty_five_cache_events_pass(self):
        for metric in ("I1mr", "ILmr"):
            with self.subTest(metric=metric):
                self.assertTrue(within_limit(metric, 62, 65))

    def test_sixty_two_to_sixty_six_cache_events_fail(self):
        for metric in ("I1mr", "ILmr"):
            with self.subTest(metric=metric):
                self.assertFalse(within_limit(metric, 62, 66))

    def test_one_thousand_to_one_thousand_twenty_nine_cache_events_pass(self):
        for metric in ("I1mr", "ILmr"):
            with self.subTest(metric=metric):
                self.assertTrue(within_limit(metric, 1000, 1029))

    def test_one_thousand_to_one_thousand_thirty_cache_events_pass(self):
        for metric in ("I1mr", "ILmr"):
            with self.subTest(metric=metric):
                self.assertTrue(within_limit(metric, 1000, 1030))

    def test_one_thousand_to_one_thousand_thirty_one_cache_events_fail(self):
        for metric in ("I1mr", "ILmr"):
            with self.subTest(metric=metric):
                self.assertFalse(within_limit(metric, 1000, 1031))

    def test_zero_cache_baseline_allows_three_and_rejects_four(self):
        for metric in ("I1mr", "ILmr"):
            with self.subTest(metric=metric):
                self.assertTrue(within_limit(metric, 0, 3))
                self.assertFalse(within_limit(metric, 0, 4))

    def test_ir_keeps_the_two_percent_relative_limit(self):
        self.assertTrue(within_limit("Ir", 1000, 1020))
        self.assertFalse(within_limit("Ir", 1000, 1021))


class SummaryComparisonTests(unittest.TestCase):
    def make_baseline(self, root, count):
        baseline_root = root / "baseline-summaries"
        manifest_path = root / "baseline-manifest.json"
        status, stdout, stderr = build_baseline_manifest(
            baseline_root, manifest_path, count
        )
        self.assertEqual(status, 0, stderr)
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        self.assertEqual(len(manifest["benchmarks"]), count)
        return manifest_path

    def compare(self, candidate_root, manifest_path):
        return run_cli(
            candidate_root,
            "--baseline-name=selected",
            f"--baseline-manifest={manifest_path}",
        )

    def test_summary_pairs_candidate_first_and_baseline_second(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            summary = summary_data(candidate=(900, 10, 10), baseline=(1000, 100, 100))
            path = write_summary(root, "ordered", summary)

            result = compare_summary(path, "selected")

            self.assertEqual(result.candidate, {"Ir": 900, "I1mr": 10, "ILmr": 10})
            self.assertEqual(result.baseline, {"Ir": 1000, "I1mr": 100, "ILmr": 100})
            self.assertTrue(result.passed)

    def test_25_baseline_and_25_candidate_pass(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 25)
            candidate_root = root / "candidate"
            write_candidate_workloads(candidate_root, 25, 25)

            status, stdout, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 0, stderr)
            self.assertIn("25/25 baseline workloads passed", stdout)
            self.assertIn("0 new candidate workloads", stdout)

    def test_25_baseline_and_36_candidate_report_11_new(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 25)
            candidate_root = root / "candidate"
            write_candidate_workloads(candidate_root, 36, 25)

            status, stdout, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 0, stderr)
            self.assertIn("25/25 baseline workloads passed", stdout)
            self.assertIn("11 new candidate workloads", stdout)
            self.assertIn("analysis::workload_11 [case_035]", stdout)

    def test_36_baseline_and_36_candidate_compare_all(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 36)
            candidate_root = root / "candidate"
            write_candidate_workloads(candidate_root, 36, 36)

            status, stdout, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 0, stderr)
            self.assertIn("36/36 baseline workloads passed", stdout)
            self.assertIn("0 new candidate workloads", stdout)

    def test_missing_baseline_id_fails_with_canonical_id(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 36)
            candidate_root = root / "candidate"
            write_candidate_workloads(candidate_root, 35, 36)

            status, _, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 2)
            self.assertIn(
                "missing baseline benchmark ID analysis::workload_11 [case_035]",
                stderr,
            )

    def test_36_baseline_and_37_candidate_report_one_new(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 36)
            candidate_root = root / "candidate"
            write_candidate_workloads(candidate_root, 37, 36)

            status, stdout, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 0, stderr)
            self.assertIn("36/36 baseline workloads passed", stdout)
            self.assertIn("1 new candidate workloads", stdout)
            self.assertIn("analysis::workload_12 [case_036]", stdout)

    def test_duplicate_baseline_summary_id_is_a_hard_error(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            baseline_root = root / "baseline-summaries"
            duplicate = left_only(workload_summary(0, baseline_name="main"))
            write_summary(baseline_root, "first", duplicate)
            write_summary(
                baseline_root,
                "duplicate",
                left_only(workload_summary(0, baseline_name="main")),
            )

            status, _, stderr = run_cli(
                baseline_root,
                f"--write-baseline-manifest={root / 'baseline-manifest.json'}",
            )

            self.assertEqual(status, 2)
            self.assertIn("duplicate baseline benchmark ID", stderr)

    def test_duplicate_candidate_id_is_a_hard_error(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 1)
            candidate_root = root / "candidate"
            duplicate = workload_summary(0)
            write_summary(candidate_root, "first", duplicate)
            write_summary(candidate_root, "duplicate", workload_summary(0))

            status, _, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 2)
            self.assertIn("duplicate candidate benchmark ID", stderr)

    def test_candidate_results_for_a_different_baseline_are_ignored(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 1)
            candidate_root = root / "candidate"
            write_summary(candidate_root, "selected", workload_summary(0))
            write_summary(
                candidate_root,
                "other",
                summary_data(
                    baseline_name="other",
                    candidate=(1000, 1000, 1000),
                    function_name="analysis::workload_00",
                    benchmark_id="case_000",
                ),
            )

            status, stdout, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 0, stderr)
            self.assertIn("1/1 baseline workloads passed", stdout)

    def test_each_missing_required_metric_is_a_hard_error(self):
        for missing_metric in METRICS:
            with (
                self.subTest(metric=missing_metric),
                tempfile.TemporaryDirectory() as temp_dir,
            ):
                root = Path(temp_dir)
                manifest = self.make_baseline(root, 1)
                candidate_root = root / "candidate"
                data = workload_summary(0)
                del data["profiles"][0]["summaries"]["parts"][0]["metrics_summary"][
                    "Callgrind"
                ][missing_metric]
                write_summary(candidate_root, "missing", data)

                status, _, stderr = self.compare(candidate_root, manifest)

                self.assertEqual(status, 2)
                self.assertIn(
                    f"missing required Callgrind metric {missing_metric}",
                    stderr,
                )

    def test_missing_paired_event_is_a_hard_error(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 1)
            candidate_root = root / "candidate"
            data = workload_summary(0)
            del data["profiles"][0]["summaries"]["parts"][0]["metrics_summary"][
                "Callgrind"
            ]["I1mr"]["metrics"]["Both"]
            write_summary(candidate_root, "missing-pair", data)

            status, _, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 2)
            self.assertIn("missing paired counts for I1mr", stderr)

    def test_malformed_paired_metrics_are_a_hard_error(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 1)
            candidate_root = root / "candidate"
            data = workload_summary(0)
            data["profiles"][0]["summaries"]["parts"][0]["metrics_summary"][
                "Callgrind"
            ]["Ir"]["metrics"]["Both"] = [{"Int": 100}]
            write_summary(candidate_root, "malformed", data)

            status, _, stderr = self.compare(candidate_root, manifest)

            self.assertEqual(status, 2)
            self.assertIn("invalid paired integer counts for Ir", stderr)

    def test_regression_in_any_baseline_workload_fails(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 36)
            candidate_root = root / "candidate"
            write_candidate_workloads(candidate_root, 36, 36, regression=35)

            status, stdout, _ = self.compare(candidate_root, manifest)

            self.assertEqual(status, 1)
            self.assertIn(
                "FAIL analysis::workload_11 [case_035]: Ir 100→103",
                stdout,
            )
            self.assertIn("35/36 baseline workloads passed", stdout)

    def test_zero_baseline_three_cache_events_pass_and_four_fail_on_summaries(self):
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            manifest = self.make_baseline(root, 1)
            candidate_root = root / "candidate"
            path = write_summary(
                candidate_root,
                "zero-to-three",
                workload_summary(0, candidate=(100, 3, 3), baseline=(100, 0, 0)),
            )
            three_event_result = compare_summary(path, "selected")

            path = write_summary(
                candidate_root,
                "zero-to-four",
                workload_summary(0, candidate=(100, 4, 4), baseline=(100, 0, 0)),
            )
            four_event_result = compare_summary(path, "selected")

            self.assertTrue(three_event_result.passed)
            self.assertFalse(four_event_result.passed)

if __name__ == "__main__":
    unittest.main()
