import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import calibration_report


SCRIPT = Path(__file__).with_name("calibration_report.py")
VARIANTS = ("a", "b", "layout", "extra_work", "extra_alloc")


def dataset(job_id="training", role="discovery", pairs=1):
    workloads = [("inlay_hints_warm", size) for size in ("small", "medium", "large")]
    workloads.append(("cold_snapshot_build_small", None))
    return {
        "format_version": 1,
        "job_id": job_id,
        "role": role,
        "source_revision": "test-revision",
        "fixture_sha256": "f" * 64,
        "harness_sha256": "e" * 64,
        "environment": {"rustc": "rustc test", "cargo": "cargo test", "iai_runner": "iai test",
                        "valgrind": "valgrind test", "os": "test-linux", "arch": "x86_64",
                        "cache_args": ["--cache-sim=yes"]},
        "host": {"runner_identity": job_id, "host_cpu": "test-cpu"},
        "variants": {name: {
            "source_sha256": ("a" if name in ("a", "b") else hex(index + 1)[2:]) * 64,
            "binary_sha256": hex(index + 1)[2:] * 64,
            "executable": f"/isolated/{name}/analysis",
            "layout_probe": {"symbol": "analysis::calibration_layout_probe", "address": "1000",
                             "size": 1024 if name == "layout" else 8},
            "layout_probe_collected": False,
        } for index, name in enumerate(VARIANTS)},
        "samples": [{
            "pair": pair, "variant": name, "order": index,
            "raw_stdout": f"raw/{pair}-{name}.stdout",
            "raw_stderr": f"raw/{pair}-{name}.stderr",
            "workloads": [{"function_name": function, "id": identifier,
                           "counts": {"Ir": 110 if name in ("extra_work", "extra_alloc") else 100,
                                      "I1mr": 100, "ILmr": 100}}
                          for function, identifier in workloads],
        } for pair in range(1, pairs + 1) for index, name in enumerate(VARIANTS)],
    }


def counts(job, variant, metric, value, pair=None):
    for sample in job["samples"]:
        if sample["variant"] == variant and (pair is None or sample["pair"] == pair):
            for workload in sample["workloads"]:
                workload["counts"][metric] = value


def write_job(root, job):
    root.mkdir(parents=True, exist_ok=True)
    for sample in job["samples"]:
        for field in ("raw_stdout", "raw_stderr"):
            path = root / sample[field]
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("retained profiler output\n", encoding="utf-8")
    (root / "data.json").write_text(json.dumps(job), encoding="utf-8")


def report_rows(report, **criteria):
    return [row for row in report["comparisons"]
            if all(row[key] == value for key, value in criteria.items())]


class CalibrationReportTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.input = self.root / "input"
        self.training = dataset()
        self.validation = dataset("holdout", "validation")

    def load(self, jobs=None):
        for index, job in enumerate(jobs or [self.training, self.validation]):
            write_job(self.input / str(index) / "nested", job)
        return calibration_report.read_datasets(self.input)

    def run_cli(self, *extra):
        return subprocess.run([
            sys.executable, str(SCRIPT), str(self.input),
            "--json-output", str(self.root / "report.json"),
            "--markdown-output", str(self.root / "report.md"), *extra,
        ], capture_output=True, text=True, check=False)

    def test_discovery_only_learning_retains_holdout_noise_and_ignores_controls(self):
        counts(self.training, "b", "I1mr", 110)
        counts(self.validation, "b", "I1mr", 130)
        for job in (self.training, self.validation):
            for variant in ("layout", "extra_work", "extra_alloc"):
                counts(job, variant, "I1mr", 10000)
        report = calibration_report.build_report(self.load())
        learned = [row for row in report["cache_allowances"] if row["metric"] == "I1mr"]
        self.assertTrue(all(row["discovery_positive_delta_floor"] == 10 for row in learned))
        held_out = report_rows(report, role="validation", variant="b", metric="I1mr")
        self.assertEqual([(row["absolute_delta"], row["proposed_allowance"], row["proposed_passed"])
                          for row in held_out], [(30, 10, False)] * 4)
        self.assertEqual(report["held_out_aa"]["by_metric"]["I1mr"]["current_false_positives"], 4)
        self.assertEqual(report["held_out_aa"]["by_metric"]["I1mr"]["proposed_false_positives"], 4)
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stderr)
        markdown = (self.root / "report.md").read_text(encoding="utf-8")
        self.assertIn("Insufficient predeclared coverage", markdown)
        self.assertIn("dependent", markdown)
        self.assertIn("| I1mr | 4 | 4 | 4 |", markdown)

    def test_learned_floor_is_inclusive_and_active_allowance_can_be_larger(self):
        counts(self.training, "b", "ILmr", 110)
        counts(self.validation, "b", "ILmr", 110)
        report = calibration_report.build_report(self.load())
        held_out = report_rows(report, role="validation", variant="b", metric="ILmr")
        self.assertTrue(all(row["proposed_passed"] and not row["current_passed"] for row in held_out))
        counts(self.validation, "a", "ILmr", 1000)
        counts(self.validation, "b", "ILmr", 1030)
        report = calibration_report.build_report(self.load())
        held_out = report_rows(report, role="validation", variant="b", metric="ILmr")
        self.assertEqual([(row["proposed_allowance"], row["proposed_passed"]) for row in held_out],
                         [(30, True)] * 4)

    def test_training_ir_exceedances_are_report_only_and_never_relax_ir(self):
        counts(self.training, "b", "Ir", 150)
        counts(self.validation, "b", "Ir", 103)
        report = calibration_report.build_report(self.load())
        ir = report_rows(report, role="validation", variant="b", metric="Ir")
        self.assertEqual([(row["active_allowance"], row["proposed_allowance"], row["proposed_passed"])
                          for row in ir], [(2, 2, False)] * 4)
        self.assertEqual(self.run_cli().returncode, 0)

    def test_positive_controls_must_exceed_two_percent_for_every_validation_signal(self):
        counts(self.validation, "extra_alloc", "Ir", 102)
        self.load()
        result = self.run_cli()
        self.assertNotEqual(result.returncode, 0)
        report = json.loads((self.root / "report.json").read_text(encoding="utf-8"))
        failures = report["sensitivity"]["validation_failures"]
        self.assertEqual({(row["variant"], row["id"]) for row in failures},
                         {("extra_alloc", size) for size in ("small", "medium", "large")})
        self.assertIn("FAIL", (self.root / "report.md").read_text(encoding="utf-8"))
        counts(self.validation, "extra_alloc", "Ir", 103)
        self.load()
        self.assertEqual(self.run_cli().returncode, 0)

    def test_discovery_control_failure_and_non_inlay_control_shifts_are_diagnostic(self):
        counts(self.training, "extra_work", "Ir", 100)
        for sample in self.validation["samples"]:
            if sample["variant"] in ("extra_work", "extra_alloc"):
                sample["workloads"][-1]["counts"]["Ir"] = 0
        report = calibration_report.build_report(self.load())
        self.assertTrue(report["sensitivity"]["passed"])
        self.assertEqual(self.run_cli().returncode, 0)

    def test_zero_baseline_is_undefined_relative_but_counts_and_policy_still_apply(self):
        for job in (self.training, self.validation):
            for variant in VARIANTS:
                counts(job, variant, "I1mr", 0)
        counts(self.validation, "b", "I1mr", 4)
        report = calibration_report.build_report(self.load())
        rows = report_rows(report, role="validation", variant="b", metric="I1mr")
        self.assertEqual([(row["absolute_delta"], row["relative_delta"], row["relative_delta_exact"],
                           row["current_passed"]) for row in rows], [(4, None, None, False)] * 4)
        summaries = [row for row in report["summaries"] if row["role"] == "validation"
                     and row["variant"] == "b" and row["metric"] == "I1mr"]
        self.assertTrue(all(row["relative_delta"]["undefined"] == 1 for row in summaries))

    def test_signed_distributions_nearest_rank_and_exact_relative_ratios(self):
        self.training = dataset(pairs=5)
        for pair, delta in enumerate((-3, -2, 0, 4, 10), 1):
            counts(self.training, "b", "ILmr", 100 + delta, pair)
        report = calibration_report.build_report(self.load())
        summary = next(row for row in report["summaries"] if row["role"] == "discovery"
                       and row["variant"] == "b" and row["metric"] == "ILmr" and row["id"] == "small")
        self.assertEqual(summary["absolute_delta"],
                         {"n": 5, "undefined": 0, "min": -3, "p50": 0, "p95": 10, "max": 10})
        self.assertEqual(summary["relative_delta_exact"]["min"], {"numerator": -3, "denominator": 100})
        self.assertEqual(summary["relative_delta_exact"]["max"], {"numerator": 1, "denominator": 10})

    def test_binary_addresses_host_identity_and_execution_order_are_preserved(self):
        self.validation["variants"]["a"]["binary_sha256"] = "9" * 64
        self.validation["variants"]["a"]["layout_probe"]["address"] = "9000"
        for sample in self.validation["samples"]:
            sample["order"] = 4 - sample["order"]
        report = calibration_report.build_report(self.load())
        job = next(job for job in report["jobs"] if job["job_id"] == "holdout")
        self.assertEqual(job["variants"]["a"]["binary_sha256"], "9" * 64)
        self.assertEqual(job["variants"]["a"]["layout_probe"]["address"], "9000")
        self.assertEqual([entry["order"] for entry in job["execution_order"]], [4, 3, 2, 1, 0])
        self.assertEqual(report["hosts"]["holdout"]["runner_identity"], "holdout")

    def test_incomparable_source_fixture_harness_environment_or_workloads_fail(self):
        changes = [
            lambda job: job.update(source_revision="other"),
            lambda job: job.update(fixture_sha256="0" * 64),
            lambda job: job.update(harness_sha256="0" * 64),
            lambda job: job["environment"].update(rustc="other compiler"),
            lambda job: job["environment"].update(cache_args=["--cache-sim=no"]),
            lambda job: job["variants"]["layout"].update(source_sha256="0" * 64),
        ]
        for change in changes:
            with self.subTest(change=change):
                original = copy.deepcopy(self.validation)
                change(self.validation)
                with self.assertRaisesRegex(ValueError, "incomparable"):
                    self.load()
                self.validation = original
        for sample in self.validation["samples"]:
            sample["workloads"][-1]["function_name"] = "unexpected_workload"
        with self.assertRaisesRegex(ValueError, "incomparable workload"):
            self.load()

    def test_malformed_counts_sources_samples_and_identities_are_rejected(self):
        mutations = [
            lambda job: job.update(format_version=True),
            lambda job: job["variants"]["b"].update(source_sha256="b" * 64),
            lambda job: job["samples"].append(copy.deepcopy(job["samples"][0])),
            lambda job: job["samples"].pop(),
            lambda job: job["samples"][0].update(pair=True),
            lambda job: job["samples"][0].update(order=True),
            lambda job: job["samples"][0].update(order=1),
            lambda job: job["samples"][0]["workloads"].pop(),
            lambda job: job["samples"][0]["workloads"].append(copy.deepcopy(job["samples"][0]["workloads"][0])),
            lambda job: job["samples"][0]["workloads"][0]["counts"].update(Ir=-1),
            lambda job: job["samples"][0]["workloads"][0]["counts"].update(I1mr=True),
            lambda job: job["samples"][0]["workloads"][0]["counts"].update(ILmr=1.5),
            lambda job: job["samples"][0]["workloads"][0]["counts"].pop("Ir"),
            lambda job: job["samples"][0]["workloads"][0]["counts"].update(Dr=10),
            lambda job: job["variants"]["layout"]["layout_probe"].update(size=1023),
            lambda job: job["variants"]["a"]["layout_probe"].update(symbol="missing_symbol"),
            lambda job: job["variants"]["a"]["layout_probe"].update(address="0"),
            lambda job: job["variants"]["layout"].update(layout_probe_collected=True),
        ]
        for change in mutations:
            with self.subTest(change=change):
                invalid = copy.deepcopy(self.training)
                change(invalid)
                with self.assertRaises(ValueError):
                    self.load([invalid, self.validation])

    def test_duplicate_jobs_and_missing_split_are_rejected(self):
        self.validation["job_id"] = "training"
        with self.assertRaisesRegex(ValueError, "duplicate job_id"):
            self.load()
        self.validation["job_id"] = "holdout"
        self.validation["role"] = "discovery"
        with self.assertRaisesRegex(ValueError, "held-out validation"):
            self.load()

    def test_recursive_reader_ignores_raw_json_but_rejects_duplicate_json_keys(self):
        self.load()
        (self.input / "irrelevant.json").write_text("not JSON", encoding="utf-8")
        self.assertEqual(len(calibration_report.read_datasets(self.input)), 2)
        path = self.input / "0" / "nested" / "data.json"
        text = path.read_text(encoding="utf-8")
        path.write_text(text.replace('"format_version": 1', '"format_version": 1, "format_version": 1'),
                        encoding="utf-8")
        result = self.run_cli()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("duplicate JSON object key", result.stderr)
        self.assertFalse((self.root / "report.json").exists())

    def test_missing_escaping_and_symlinked_raw_evidence_is_rejected(self):
        self.load()
        root = self.input / "0" / "nested"
        path = root / self.training["samples"][0]["raw_stdout"]
        path.unlink()
        with self.assertRaisesRegex(ValueError, "evidence file"):
            calibration_report.read_datasets(self.input)
        outside = self.root / "outside.log"
        outside.write_text("outside", encoding="utf-8")
        path.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "evidence file"):
            calibration_report.read_datasets(self.input)
        self.training["samples"][0]["raw_stdout"] = "../outside.log"
        (root / "data.json").write_text(json.dumps(self.training), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "must not escape"):
            calibration_report.read_datasets(self.input)

    def test_optional_expected_coverage_fails_missing_matrix_jobs_or_pairs(self):
        self.load()
        result = self.run_cli("--expected-discovery-jobs", "1", "--expected-validation-jobs", "1",
                              "--expected-pairs", "1")
        self.assertEqual(result.returncode, 0, result.stderr)
        for options in (("--expected-discovery-jobs", "7"),
                        ("--expected-validation-jobs", "3"), ("--expected-pairs", "5"),
                        ("--expected-pairs", "0")):
            with self.subTest(options=options):
                self.assertNotEqual(self.run_cli(*options).returncode, 0)

    def test_missing_required_signals_and_noncontiguous_pairs_fail_closed(self):
        for sample in self.training["samples"]:
            sample["workloads"] = [workload for workload in sample["workloads"]
                                   if workload["id"] != "medium"]
        with self.assertRaisesRegex(ValueError, "missing small/medium/large"):
            self.load()
        self.training = dataset()
        for sample in self.training["samples"]:
            sample["pair"] = 2
        with self.assertRaisesRegex(ValueError, "contiguous"):
            self.load()

    def test_zero_ir_positive_controls_require_a_real_increase(self):
        for job in (self.training, self.validation):
            for variant in VARIANTS:
                counts(job, variant, "Ir", 0)
            for variant in ("extra_work", "extra_alloc"):
                counts(job, variant, "Ir", 1)
        report = calibration_report.build_report(self.load())
        self.assertTrue(report["sensitivity"]["passed"])
        counts(self.validation, "extra_work", "Ir", 0)
        report = calibration_report.build_report(self.load())
        self.assertFalse(report["sensitivity"]["passed"])
        self.assertEqual(len(report["sensitivity"]["validation_failures"]), 3)

    def test_predeclared_coverage_requires_full_seven_three_five_design(self):
        jobs = [dataset(f"{role}-{index}", role, pairs=5)
                for role, count in (("discovery", 7), ("validation", 3)) for index in range(count)]
        report = calibration_report.build_report(self.load(jobs))
        self.assertTrue(report["coverage"]["predeclared_coverage_met"])
        self.assertIn("do not establish tail reliability", report["coverage"]["limitation"])


if __name__ == "__main__":
    unittest.main()
