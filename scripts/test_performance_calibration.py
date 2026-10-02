import copy
import json
from pathlib import Path
import tempfile
import unittest

from performance_calibration import (
    METRICS,
    PROBE,
    VARIANTS,
    balanced_order,
    check_probe_absent,
    parse_executable,
    parse_measurement,
    parse_probe_symbols,
    parse_workload_list,
    source_manifest,
    strict_json,
    validate_directories,
    variant_harness,
)


EXECUTABLE = Path("/calibration/a/target/release/deps/analysis-actual")
IDENTITY = ("inlay_hints_warm", "small")


def fresh_summary(counts=(100, 0, 3)):
    return {
        "function_name": IDENTITY[0],
        "id": IDENTITY[1],
        "baselines": ["fresh123", "fresh123"],
        "kind": "LibraryBenchmark",
        "benchmark_exe": str(EXECUTABLE),
        "profiles": [{
            "tool": "Callgrind",
            "summaries": {"parts": [{"metrics_summary": {"Callgrind": {
                metric: {"metrics": {"Left": {"Int": value}}}
                for metric, value in zip(METRICS, counts)
            }}}]},
        }],
    }


def measured(*summaries, expected=None):
    return parse_measurement(
        "\n".join(json.dumps(summary) for summary in summaries),
        "fresh123", EXECUTABLE, {IDENTITY} if expected is None else expected,
    )[0]


class MeasurementIntegrityTests(unittest.TestCase):
    def test_zero_is_a_real_count_not_a_missing_measurement(self):
        self.assertEqual(measured(fresh_summary((0, 0, 0))), [{
            "function_name": "inlay_hints_warm", "id": "small",
            "counts": {"Ir": 0, "I1mr": 0, "ILmr": 0},
        }])

    def test_invalid_count_types_and_signs_are_rejected(self):
        for index in range(3):
            for invalid in (-1, True, False, 1.0, "1", None):
                with self.subTest(metric=METRICS[index], invalid=invalid):
                    counts = [100, 2, 3]
                    counts[index] = invalid
                    with self.assertRaisesRegex(ValueError, "nonnegative integer"):
                        measured(fresh_summary(counts))

    def test_missing_duplicate_and_extra_workloads_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "missing workloads"):
            measured()
        with self.assertRaisesRegex(ValueError, "duplicate or unexpected"):
            measured(fresh_summary(), fresh_summary())
        extra = fresh_summary()
        extra["id"] = "unexpected"
        with self.assertRaisesRegex(ValueError, "duplicate or unexpected"):
            measured(extra)
        with self.assertRaisesRegex(ValueError, "missing workloads"):
            measured(fresh_summary(), expected={IDENTITY, ("inlay_hints_warm", "medium")})

    def test_null_workload_id_is_distinct_from_named_id(self):
        summary = fresh_summary()
        summary["id"] = None
        self.assertEqual(measured(summary, expected={(IDENTITY[0], None)})[0]["id"], None)
        with self.assertRaisesRegex(ValueError, "duplicate or unexpected"):
            measured(summary)

    def test_stale_baseline_or_other_executable_cannot_supply_metrics(self):
        for field, value in (
            ("baselines", [None, "fresh123"]),
            ("baselines", ["stale", "stale"]),
            ("benchmark_exe", "/old/analysis"),
            ("kind", "BinaryBenchmark"),
        ):
            with self.subTest(field=field, value=value):
                summary = fresh_summary()
                summary[field] = value
                with self.assertRaises(ValueError):
                    measured(summary)

    def test_comparison_metrics_cannot_be_mistaken_for_fresh_counts(self):
        for variant in ({"Both": [{"Int": 100}, {"Int": 100}]}, {"Right": {"Int": 100}}, {"Left": {"Int": 100}, "Both": [{"Int": 100}, {"Int": 100}]}):
            summary = fresh_summary()
            summary["profiles"][0]["summaries"]["parts"][0]["metrics_summary"]["Callgrind"]["Ir"]["metrics"] = variant
            with self.assertRaisesRegex(ValueError, "Left-only"):
                measured(summary)

    def test_missing_metrics_and_unexpected_profile_parts_fail_closed(self):
        for mutation in ("metric", "part", "tool", "profile", "malformed"):
            with self.subTest(mutation=mutation):
                summary = fresh_summary()
                profile = summary["profiles"][0]
                if mutation == "metric":
                    del profile["summaries"]["parts"][0]["metrics_summary"]["Callgrind"]["ILmr"]
                elif mutation == "part":
                    profile["summaries"]["parts"].append(copy.deepcopy(profile["summaries"]["parts"][0]))
                elif mutation == "tool":
                    profile["tool"] = "Cachegrind"
                elif mutation == "malformed":
                    summary["profiles"] = [True]
                else:
                    summary["profiles"].append(copy.deepcopy(profile))
                with self.assertRaises(ValueError):
                    measured(summary)

    def test_non_json_transport_output_and_duplicate_keys_fail_closed(self):
        with self.assertRaises(ValueError):
            parse_measurement("profiler failed\n" + json.dumps(fresh_summary()), "fresh123", EXECUTABLE, {IDENTITY})
        for text in ('{"Ir": 1, "Ir": 2}', '{"count": NaN}', '{"count": Infinity}'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                strict_json(text)


class LayoutIntegrityTests(unittest.TestCase):
    def test_real_nm_format_reports_address_size_and_demangled_name(self):
        self.assertEqual(parse_probe_symbols(
            f"0000000000012340 0000000000000400 t analysis::{PROBE}\n", True,
        ), {"symbol": f"analysis::{PROBE}", "address": "0000000000012340", "size": 1024})

    def test_unretained_ambiguous_small_or_unsupported_symbols_fail(self):
        record = f"00012340 00000400 t analysis::{PROBE}\n"
        for text in ("", record + record, record.replace("00000400", "000003ff"), record.replace("00012340", "00000000"), record.replace(" t ", " U "), record.replace(PROBE, PROBE + "_other"), f"00012340 t analysis::{PROBE}\n"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                parse_probe_symbols(text, True)
        with self.assertRaises(ValueError):
            parse_probe_symbols(record.replace("00000400", "00000000"), False)

    def test_collected_probe_and_malformed_profiles_fail(self):
        header = "# callgrind format\nversion: 1\nevents: Ir I1mr ILmr\n"
        check_probe_absent(header + "fn=(1) analysis::inlay_hints_warm\n1 100 2 3\n")
        for text in (
            header + f"fn=(2) analysis::{PROBE}\n1 1 0 0\n",
            header + f"cfn=(2) analysis::{PROBE}\ncalls=1 1\n1 1 0 0\n",
            "", "events: Ir\n", header.replace("events: Ir", "events: Dr"),
        ):
            with self.subTest(text=text), self.assertRaises(ValueError):
                check_probe_absent(text)

    def test_exact_known_anchors_reject_harness_drift_and_ambiguity(self):
        harness = (Path(__file__).resolve().parents[1] / "benches" / "analysis.rs").read_text(encoding="utf-8")
        alterations = (
            harness.replace("fn inlay_hints_warm(", "fn renamed_inlay_hints_warm("),
            harness.replace('identifier_name: "transform_31"', 'identifier_name: "different"'),
            harness.replace("fn medium() -> Self", "fn medium_changed() -> Self"),
            harness.replace("fn large() -> Self", "fn large_changed() -> Self"),
            harness + harness,
            harness + f"\nfn {PROBE}(state: u64) -> u64 {{ state }}\n",
        )
        for variant in VARIANTS:
            for altered in alterations:
                with self.subTest(variant=variant), self.assertRaises(ValueError):
                    variant_harness(altered, variant)


class ArtifactAndPlacementTests(unittest.TestCase):
    def test_cargo_artifact_is_required_and_cannot_escape_independent_target(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "target"
            target.mkdir()
            executable = target / "compiler-chosen-name"
            executable.write_bytes(b"executable artifact")
            executable.chmod(0o755)
            artifact = {"reason": "compiler-artifact", "target": {"name": "analysis", "kind": ["bench"]}, "executable": str(executable)}
            finished = {"reason": "build-finished", "success": True}
            messages = lambda *items: "\n".join(json.dumps(item) for item in items)
            self.assertEqual(parse_executable(messages(artifact, finished), root, target), executable.resolve())
            outside = dict(artifact, executable=str(root / "old-executable"))
            for text in (messages(finished), messages(artifact), messages(artifact, artifact, finished), messages(outside, finished), messages(artifact, dict(finished, success=False))):
                with self.subTest(text=text), self.assertRaises(ValueError):
                    parse_executable(text, root, target)

    def test_each_five_pair_block_balances_all_execution_positions(self):
        for job in ("discovery1", "validation3", "local"):
            for first in (1, 6):
                orders = [balanced_order(job, pair) for pair in range(first, first + 5)]
                for variant in VARIANTS:
                    self.assertEqual(sorted(order.index(variant) for order in orders), list(range(5)))

    def test_nested_or_stale_destinations_cannot_contaminate_source_or_output(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            work = root / "work"
            output = root / "output"
            validate_directories(source, work, output)
            # Disjoint children are safe: excluded source subtrees do not recurse.
            validate_directories(source, source / "work", source / "output")
            for invalid_work, invalid_output in ((root, output), (work, root), (work, work / "raw"), (output / "build", output), (source, output)):
                with self.subTest(work=invalid_work, output=invalid_output), self.assertRaises(ValueError):
                    validate_directories(source, invalid_work, invalid_output)
            work.mkdir()
            (work / "old-binary").write_bytes(b"stale")
            with self.assertRaisesRegex(ValueError, "stale"):
                validate_directories(source, work, output)

    def test_snapshot_rejects_symlinks_to_mutable_external_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            external = root / "changing.rs"
            external.write_text("external mutable source", encoding="utf-8")
            (source / "library.rs").symlink_to(external)
            with self.assertRaisesRegex(ValueError, "symlinks"):
                source_manifest(source)

    def test_executed_workload_manifest_rejects_incomplete_or_duplicate_list(self):
        lines = [f"analysis::analysis_hot_paths::inlay_hints_warm::{size}: benchmark" for size in ("small", "medium", "large")]
        valid = "\n".join(lines + ["", "0 tests, 3 benchmarks"])
        self.assertEqual(parse_workload_list(valid), {("inlay_hints_warm", size) for size in ("small", "medium", "large")})
        for text in (valid.replace("3 benchmarks", "4 benchmarks"), valid + "\n" + lines[0], "\n".join(lines[:2] + ["0 tests, 2 benchmarks"]), valid + "\nprofiler warning"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                parse_workload_list(text)


if __name__ == "__main__":
    unittest.main()
