#!/usr/bin/env python3
"""Validate paired LSP measurements and report latency changes without thresholds."""

import argparse
import json
from pathlib import Path
import re
from statistics import median


WORKLOADS = (
    "hover_warm",
    "definition_warm",
    "completion_warm",
    "open_to_hover_large",
    "edit_to_hover_large",
    "hover_during_large_edit",
    "hover_during_large_open",
)
PAIRED_METADATA = ("platform", "machine", "python", "rounds", "samples", "warmup")


def validate_measurement(data, source="measurement"):
    """Validate the v1 measurement schema, allowing descriptive metadata extras."""
    def require(condition, message):
        if not condition:
            raise ValueError(f"{source}: {message}")

    require(isinstance(data, dict), "measurement must be an object")
    require(type(data.get("format_version")) is int and data["format_version"] == 1,
            "format_version must be integer 1")
    digest = data.get("workload_digest")
    require(isinstance(digest, str) and re.fullmatch(r"[0-9a-f]{64}", digest) is not None,
            "workload_digest must be a lowercase SHA256")
    metadata = data.get("metadata")
    require(isinstance(metadata, dict), "metadata must be an object")
    binary_hash = metadata.get("binary_sha256")
    require(isinstance(binary_hash, str) and re.fullmatch(r"[0-9a-f]{64}", binary_hash) is not None,
            "metadata.binary_sha256 must be a lowercase SHA256")
    for field in ("platform", "machine", "python"):
        require(isinstance(metadata.get(field), str) and bool(metadata[field].strip()),
                f"metadata.{field} must be a nonempty string")
    for field in ("rounds", "samples", "warmup"):
        value = metadata.get(field)
        minimum = 0 if field == "warmup" else 1
        require(type(value) is int and value >= minimum,
                f"metadata.{field} must be an integer >= {minimum}")
    workloads = data.get("workloads")
    require(isinstance(workloads, dict), "workloads must be an object")
    require(set(workloads) == set(WORKLOADS), "workloads must contain exactly the seven supported IDs")
    for name in WORKLOADS:
        workload = workloads[name]
        require(isinstance(workload, dict), f"{name} must be an object")
        rounds = workload.get("rounds_ns")
        require(isinstance(rounds, list) and len(rounds) == metadata["rounds"],
                f"{name}.rounds_ns must match metadata.rounds")
        for index, samples in enumerate(rounds):
            require(isinstance(samples, list) and len(samples) == metadata["samples"],
                    f"{name} round {index + 1} must match metadata.samples")
            require(all(type(value) is int and value > 0 for value in samples),
                    f"{name} round {index + 1} samples must be positive integer nanoseconds")
    return data


def round_percentiles(samples):
    ordered = sorted(samples)
    return {
        f"p{percentile}_ns": ordered[(percentile * len(ordered) + 99) // 100 - 1]
        for percentile in (50, 95)
    }


def summarize(rounds):
    values = [round_percentiles(samples) for samples in rounds]
    return {
        "rounds": values,
        **{key: median(value[key] for value in values) for key in ("p50_ns", "p95_ns")},
    }


def delta(baseline, candidate):
    result = {}
    for percentile in (50, 95):
        key = f"p{percentile}_ns"
        difference = candidate[key] - baseline[key]
        result[key] = difference
        result[f"p{percentile}_percent"] = difference / baseline[key] * 100
    return result


def compare_measurements(baseline, candidate):
    validate_measurement(baseline, "baseline")
    validate_measurement(candidate, "candidate")
    if baseline["workload_digest"] != candidate["workload_digest"]:
        raise ValueError("baseline and candidate workload_digest differ")
    for field in PAIRED_METADATA:
        if baseline["metadata"][field] != candidate["metadata"][field]:
            raise ValueError(f"baseline and candidate metadata.{field} differ")
    report = {
        "format_version": 1,
        "mode": "report-only",
        "workload_digest": baseline["workload_digest"],
        "baseline_metadata": baseline["metadata"],
        "candidate_metadata": candidate["metadata"],
        "workloads": {},
    }
    for name in WORKLOADS:
        left = summarize(baseline["workloads"][name]["rounds_ns"])
        right = summarize(candidate["workloads"][name]["rounds_ns"])
        changes = delta(left, right)
        changes["rounds"] = [delta(a, b) for a, b in zip(left["rounds"], right["rounds"])]
        report["workloads"][name] = {"baseline": left, "candidate": right, "delta": changes}
    return report


def render_markdown(report):
    lines = [
        "# End-to-end LSP latency",
        "",
        "Report-only: numerical latency changes never fail CI; invalid or incomparable measurements do.",
        "",
        "Times are milliseconds. Each round uses nearest-rank p50/p95; aggregates are the median of round percentiles, not pooled requests. Deltas are candidate minus baseline.",
        "",
        "| Workload | Baseline p50 (ms) | Candidate p50 (ms) | p50 change | Baseline p95 (ms) | Candidate p95 (ms) | p95 change |",
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: |",
    ]
    for name, workload in report["workloads"].items():
        cells = [name]
        for percentile in (50, 95):
            key = f"p{percentile}_ns"
            cells.extend([
                f'{workload["baseline"][key] / 1_000_000:.3f}',
                f'{workload["candidate"][key] / 1_000_000:.3f}',
                f'{workload["delta"][f"p{percentile}_percent"]:+.2f}%',
            ])
        lines.append("| " + " | ".join(cells) + " |")
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline_json", type=Path)
    parser.add_argument("candidate_json", type=Path)
    parser.add_argument("--json-output", type=Path, required=True)
    parser.add_argument("--markdown-output", type=Path, required=True)
    args = parser.parse_args()
    try:
        baseline = json.loads(args.baseline_json.read_text(encoding="utf-8"))
        candidate = json.loads(args.candidate_json.read_text(encoding="utf-8"))
        report = compare_measurements(baseline, candidate)
        json_text = json.dumps(report, indent=2, allow_nan=False) + "\n"
        markdown = render_markdown(report)
        args.json_output.write_text(json_text, encoding="utf-8")
        args.markdown_output.write_text(markdown, encoding="utf-8")
    except (OSError, ValueError) as error:
        parser.error(f"cannot compare LSP latency measurements: {error}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
