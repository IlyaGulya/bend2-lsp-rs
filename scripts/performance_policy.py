#!/usr/bin/env python3
"""Generate baseline manifests and compare Iai summaries under active policy."""

import argparse
from dataclasses import dataclass
import json
from pathlib import Path
from typing import Any

METRICS = ("Ir", "I1mr", "ILmr")


@dataclass(frozen=True)
class WorkloadResult:
    name: str
    baseline: dict[str, int]
    candidate: dict[str, int]
    passed: bool


def within_limit(metric: str, baseline: int, candidate: int) -> bool:
    """Return whether candidate is within the active limit for one event."""
    if metric not in METRICS:
        raise ValueError(f"unsupported Callgrind metric: {metric}")
    if (
        type(baseline) is not int
        or type(candidate) is not int
        or baseline < 0
        or candidate < 0
    ):
        raise ValueError("Callgrind counts must be non-negative integers")

    if metric == "Ir":
        return candidate * 100 <= baseline * 102

    allowed_increase = max((baseline * 3 + 99) // 100, 3)
    return candidate <= baseline + allowed_increase


def _callgrind_metrics(summary: dict[str, Any]) -> dict[str, Any]:
    return summary["profiles"][0]["summaries"]["parts"][0]["metrics_summary"][
        "Callgrind"
    ]


def _paired_counts(metrics: Any) -> dict[str, tuple[int, int]] | None:
    if not isinstance(metrics, dict):
        raise ValueError("Callgrind metrics are not an object")

    metric_values: dict[str, dict[str, Any]] = {}
    for metric in METRICS:
        metric_data = metrics.get(metric)
        if not isinstance(metric_data, dict):
            raise ValueError(f"missing required Callgrind metric {metric}")
        values = metric_data.get("metrics")
        if not isinstance(values, dict):
            raise ValueError(f"malformed paired metrics for {metric}")
        metric_values[metric] = values

    # Iai emits candidate-only workloads with Left metrics and no Both pair.
    if all(
        "Both" not in values
        and isinstance(values.get("Left"), dict)
        and type(values["Left"].get("Int")) is int
        and values["Left"]["Int"] >= 0
        for values in metric_values.values()
    ):
        return None

    pairs: dict[str, tuple[int, int]] = {}
    for metric, values in metric_values.items():
        pair = values.get("Both")
        if pair is None:
            raise ValueError(f"missing paired counts for {metric}")
        if (
            not isinstance(pair, list)
            or len(pair) != 2
            or any(
                not isinstance(value, dict)
                or type(value.get("Int")) is not int
                or value["Int"] < 0
                for value in pair
            )
        ):
            raise ValueError(f"invalid paired integer counts for {metric}")
        pairs[metric] = (pair[0]["Int"], pair[1]["Int"])
    return pairs


def _canonical_id(summary: Any) -> tuple[str, str | None]:
    if not isinstance(summary, dict):
        raise ValueError("benchmark summary must be an object")
    function_name = summary.get("function_name")
    benchmark_id = summary.get("id")
    if not isinstance(function_name, str) or not function_name:
        raise ValueError("benchmark function_name must be a non-empty string")
    if benchmark_id is not None and (
        not isinstance(benchmark_id, str) or not benchmark_id
    ):
        raise ValueError("benchmark id must be null or a non-empty string")
    return function_name, benchmark_id


def _display_id(benchmark_id: tuple[str, str | None]) -> str:
    function_name, workload_id = benchmark_id
    return f"{function_name} [{workload_id}]" if workload_id is not None else function_name


def _manifest_entry(benchmark_id: tuple[str, str | None]) -> dict[str, Any]:
    return {"function_name": benchmark_id[0], "id": benchmark_id[1]}


def _sorted_ids(benchmark_ids: set[tuple[str, str | None]]) -> list[tuple[str, str | None]]:
    return sorted(benchmark_ids, key=lambda item: (item[0], item[1] or ""))


def _load_baseline_manifest(path: Path) -> list[tuple[str, str | None]]:
    manifest = json.loads(path.read_text(encoding="utf-8"))
    if (
        not isinstance(manifest, dict)
        or type(manifest.get("format_version")) is not int
        or manifest["format_version"] != 1
        or not isinstance(manifest.get("benchmarks"), list)
    ):
        raise ValueError("baseline manifest must use format_version 1 and list benchmarks")

    benchmark_ids = set()
    for entry in manifest["benchmarks"]:
        benchmark_id = _canonical_id(entry)
        if benchmark_id in benchmark_ids:
            raise ValueError(
                f"duplicate baseline benchmark ID {_display_id(benchmark_id)}"
            )
        benchmark_ids.add(benchmark_id)
    if not benchmark_ids:
        raise ValueError("baseline manifest contains no benchmarks")
    return _sorted_ids(benchmark_ids)


def write_baseline_manifest(root: Path, path: Path) -> int:
    benchmark_ids = set()
    for summary_path in sorted(root.rglob("summary.json")):
        summary = json.loads(summary_path.read_text(encoding="utf-8"))
        benchmark_id = _canonical_id(summary)
        if benchmark_id in benchmark_ids:
            raise ValueError(
                f"duplicate baseline benchmark ID {_display_id(benchmark_id)} "
                f"in {summary_path}"
            )
        benchmark_ids.add(benchmark_id)

    if not benchmark_ids:
        raise ValueError(f"no Iai summary.json files found under {root}")

    manifest = {
        "format_version": 1,
        "benchmarks": [
            _manifest_entry(benchmark_id) for benchmark_id in _sorted_ids(benchmark_ids)
        ],
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
    print(f"Wrote baseline manifest with {len(benchmark_ids)} unique benchmarks.")
    return 0


def _has_selected_baseline(summary: Any, baseline_name: str) -> bool:
    if not isinstance(summary, dict):
        raise ValueError("benchmark summary must be an object")
    return summary.get("baselines") == [None, baseline_name]


def _result_from_pairs(
    benchmark_id: tuple[str, str | None],
    pairs: dict[str, tuple[int, int]],
) -> WorkloadResult:
    baseline = {metric: pairs[metric][1] for metric in METRICS}
    candidate = {metric: pairs[metric][0] for metric in METRICS}
    passed = all(
        within_limit(metric, baseline[metric], candidate[metric])
        for metric in METRICS
    )
    return WorkloadResult(_display_id(benchmark_id), baseline, candidate, passed)


def _compare_summary_data(
    summary: dict[str, Any], baseline_name: str
) -> WorkloadResult | None:
    if not _has_selected_baseline(summary, baseline_name):
        return None
    benchmark_id = _canonical_id(summary)
    pairs = _paired_counts(_callgrind_metrics(summary))
    if pairs is None:
        return None
    return _result_from_pairs(benchmark_id, pairs)


def compare_summary(path: Path, baseline_name: str) -> WorkloadResult | None:
    summary = json.loads(path.read_text(encoding="utf-8"))
    return _compare_summary_data(summary, baseline_name)


def compare_tree(
    root: Path,
    baseline_name: str,
    baseline_ids: list[tuple[str, str | None]],
) -> tuple[list[WorkloadResult], list[tuple[str, str | None]]]:
    selected: dict[tuple[str, str | None], dict[str, Any]] = {}
    for path in sorted(root.rglob("summary.json")):
        summary = json.loads(path.read_text(encoding="utf-8"))
        if not _has_selected_baseline(summary, baseline_name):
            continue
        benchmark_id = _canonical_id(summary)
        if benchmark_id in selected:
            raise ValueError(
                f"duplicate candidate benchmark ID {_display_id(benchmark_id)} "
                f"in {path}"
            )
        selected[benchmark_id] = summary

    baseline_id_set = set(baseline_ids)
    candidate_pairs = {
        benchmark_id: _paired_counts(_callgrind_metrics(summary))
        for benchmark_id, summary in selected.items()
    }

    missing = [
        benchmark_id
        for benchmark_id in baseline_ids
        if benchmark_id not in selected or candidate_pairs[benchmark_id] is None
    ]
    if missing:
        raise ValueError(
            "; ".join(
                f"missing baseline benchmark ID {_display_id(benchmark_id)}"
                for benchmark_id in missing
            )
        )

    new_ids = [
        benchmark_id
        for benchmark_id in _sorted_ids(set(selected) - baseline_id_set)
    ]
    paired_new = [
        benchmark_id
        for benchmark_id in new_ids
        if candidate_pairs[benchmark_id] is not None
    ]
    if paired_new:
        raise ValueError(
            "; ".join(
                "candidate benchmark is paired but absent from baseline manifest: "
                f"{_display_id(benchmark_id)}"
                for benchmark_id in paired_new
            )
        )

    results = [
        _result_from_pairs(benchmark_id, candidate_pairs[benchmark_id])
        for benchmark_id in baseline_ids
    ]
    return results, new_ids


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "summary_root",
        type=Path,
        help="root containing Iai summary.json files",
    )
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument(
        "--write-baseline-manifest",
        type=Path,
        help="write the unique benchmark IDs found in a base-run summary tree",
    )
    mode.add_argument(
        "--baseline-manifest",
        type=Path,
        help="authoritative benchmark IDs emitted by the base run",
    )
    parser.add_argument(
        "--baseline-name",
        help="name used by Iai's --baseline option when comparing candidate summaries",
    )
    args = parser.parse_args()

    if not args.summary_root.is_dir():
        parser.error(f"summary root is not a directory: {args.summary_root}")

    if args.write_baseline_manifest is not None:
        try:
            return write_baseline_manifest(args.summary_root, args.write_baseline_manifest)
        except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
            parser.error(f"cannot write baseline manifest: {error}")

    if not args.baseline_name:
        parser.error("--baseline-name is required with --baseline-manifest")

    try:
        baseline_ids = _load_baseline_manifest(args.baseline_manifest)
        results, new_ids = compare_tree(
            args.summary_root,
            args.baseline_name,
            baseline_ids,
        )
    except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
        parser.error(f"cannot compare Iai summaries: {error}")

    passed = sum(result.passed for result in results)
    for result in results:
        if result.passed:
            continue
        failed_metrics = [
            f"{metric} {result.baseline[metric]}→{result.candidate[metric]}"
            for metric in METRICS
            if not within_limit(metric, result.baseline[metric], result.candidate[metric])
        ]
        print(f"FAIL {result.name}: {', '.join(failed_metrics)}")

    print(f"{passed}/{len(results)} baseline workloads passed.")
    print(f"{len(new_ids)} new candidate workloads:")
    for benchmark_id in new_ids:
        print(f"NEW {_display_id(benchmark_id)}")
    return int(passed != len(results))


if __name__ == "__main__":
    raise SystemExit(main())
