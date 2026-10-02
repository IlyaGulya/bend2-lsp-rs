#!/usr/bin/env python3
"""Validate calibration datasets and propose allowances without changing active gates."""

import argparse
from collections import defaultdict
from fractions import Fraction
import json
from pathlib import Path
import re
import sys

from performance_policy import METRICS, _canonical_id, within_limit


VARIANTS = ("a", "b", "layout", "extra_work", "extra_alloc")
CONTROLS = VARIANTS[1:]
ROLES = ("discovery", "validation")
ENVIRONMENT_FIELDS = ("rustc", "cargo", "iai_runner", "valgrind", "os", "arch", "cache_args")
SIGNALS = {("inlay_hints_warm", size) for size in ("small", "medium", "large")}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def nonempty(value):
    return isinstance(value, str) and bool(value.strip())


def digest(value):
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def unique_object(items):
    result = {}
    for key, value in items:
        require(key not in result, f"duplicate JSON object key: {key}")
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError(f"nonfinite JSON number: {value}")


def validate_job(data, root, source):
    """Validate a complete job, including the retained raw evidence references."""
    def check(condition, message):
        require(condition, f"{source}: {message}")

    check(isinstance(data, dict), "dataset must be an object")
    check(type(data.get("format_version")) is int and data["format_version"] == 1,
          "format_version must be integer 1")
    for field in ("job_id", "source_revision"):
        check(nonempty(data.get(field)), f"{field} must be a nonempty string")
    check(data.get("role") in ROLES, "role must be discovery or validation")
    for field in ("fixture_sha256", "harness_sha256"):
        check(digest(data.get(field)), f"{field} must be a lowercase SHA256")
    environment = data.get("environment")
    check(isinstance(environment, dict), "environment must be an object")
    for field in ENVIRONMENT_FIELDS:
        value = environment.get(field)
        if field == "cache_args":
            check(isinstance(value, list) and bool(value) and all(nonempty(arg) for arg in value),
                  "environment.cache_args must be a nonempty string list")
        else:
            check(nonempty(value), f"environment.{field} must be a nonempty string")
    check(set(ENVIRONMENT_FIELDS) <= set(environment) <= set(ENVIRONMENT_FIELDS) | {"cache_geometry"},
          "environment must contain only comparable toolchain/cache fields; put host metadata in host")
    check(isinstance(data.get("host", {}), dict), "host must be an object when present")
    variants = data.get("variants")
    check(isinstance(variants, dict) and set(variants) == set(VARIANTS),
          "variants must contain exactly a, b, layout, extra_work, extra_alloc")
    for name, variant in variants.items():
        check(isinstance(variant, dict), f"variant {name} must be an object")
        for field in ("source_sha256", "binary_sha256"):
            check(digest(variant.get(field)), f"variant {name}.{field} must be a lowercase SHA256")
        check(nonempty(variant.get("executable")), f"variant {name}.executable must be nonempty")
        probe = variant.get("layout_probe")
        check(isinstance(probe, dict), f"variant {name}.layout_probe must be an object")
        check(nonempty(probe.get("symbol")) and
              probe["symbol"].split("::")[-1] == "calibration_layout_probe",
              f"variant {name} must retain calibration_layout_probe")
        address = probe.get("address")
        check(isinstance(address, str) and re.fullmatch(r"[0-9a-fA-F]+", address) is not None
              and int(address, 16) > 0, f"variant {name} probe address must be nonzero hexadecimal")
        size = probe.get("size")
        check(type(size) is int and size >= (1024 if name == "layout" else 1),
              f"variant {name} probe size is invalid")
        check("layout_probe_collected" not in variant or variant["layout_probe_collected"] is False,
              f"variant {name} layout probe must have no collected execution")
    check(variants["a"]["source_sha256"] == variants["b"]["source_sha256"],
          "A/A source hashes differ")
    samples = data.get("samples")
    check(isinstance(samples, list) and bool(samples), "samples must be a nonempty list")
    seen = set()
    orders = defaultdict(set)
    identities = None
    for sample in samples:
        check(isinstance(sample, dict), "sample must be an object")
        pair, variant, order = sample.get("pair"), sample.get("variant"), sample.get("order")
        check(type(pair) is int and pair >= 1, "pair must be a positive integer")
        check(variant in VARIANTS, "sample variant is unsupported")
        check(type(order) is int and order >= 0, "order must be a nonnegative integer")
        check((pair, variant) not in seen, "duplicate (pair, variant) sample")
        check(order not in orders[pair], "duplicate execution order within pair")
        seen.add((pair, variant))
        orders[pair].add(order)
        for field in ("raw_stdout", "raw_stderr"):
            value = sample.get(field)
            check(nonempty(value), f"sample {field} must be a relative path")
            path = Path(value)
            check(not path.is_absolute() and ".." not in path.parts,
                  f"sample {field} must not escape the dataset directory")
            resolved = (root / path).resolve()
            check(resolved.is_relative_to(root.resolve()) and resolved.is_file(),
                  f"sample {field} evidence file is missing or escapes the dataset directory")
        workloads = sample.get("workloads")
        check(isinstance(workloads, list) and bool(workloads), "workloads must be a nonempty list")
        current = set()
        for workload in workloads:
            check(isinstance(workload, dict), "workload must be an object")
            name, identifier = _canonical_id(workload)
            check(nonempty(name), "workload function_name must be nonempty")
            check("id" in workload and (identifier is None or nonempty(identifier)),
                  "workload id must be null or a nonempty string")
            identity = (name, identifier)
            check(identity not in current, "duplicate workload identity")
            current.add(identity)
            counts = workload.get("counts")
            check(isinstance(counts, dict) and set(counts) == set(METRICS),
                  "workload counts must contain exactly Ir, I1mr, ILmr")
            check(all(type(value) is int and value >= 0 for value in counts.values()),
                  "metric counts must be nonnegative integers (not bool)")
        if identities is None:
            identities = current
        check(current == identities, "workload identities differ between samples")
    check(SIGNALS <= identities, "missing small/medium/large inlay_hints_warm controls")
    pairs = {pair for pair, _ in seen}
    check(min(pairs) == 1 and max(pairs) == len(pairs), "pair numbers must be contiguous from 1")
    check(seen == {(pair, name) for pair in pairs for name in VARIANTS},
          "every pair must contain all five variants")
    return data


def read_datasets(root):
    require(root.is_dir(), "input root must be a directory")
    paths = sorted(root.rglob("data.json"))
    require(bool(paths), "no data.json datasets found")
    jobs = []
    for path in paths:
        try:
            require(path.resolve().is_relative_to(root.resolve()),
                    "data.json must not escape the input root")
            data = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object,
                              parse_constant=reject_constant)
            jobs.append(validate_job(data, path.parent, str(path)))
        except (OSError, UnicodeError, ValueError) as error:
            raise ValueError(f"{path}: {error}") from error
    validate_comparability(jobs)
    return jobs


def validate_comparability(jobs):
    require(bool(jobs), "no calibration jobs")
    require(len({job["job_id"] for job in jobs}) == len(jobs), "duplicate job_id")
    require({job["role"] for job in jobs} == set(ROLES),
            "both discovery and held-out validation jobs are required")
    first = jobs[0]
    for job in jobs[1:]:
        for field in ("source_revision", "fixture_sha256", "harness_sha256"):
            require(job[field] == first[field], f"incomparable {field} across jobs")
        require(job["environment"] == first["environment"], "incomparable environment across jobs")
        for variant in VARIANTS:
            require(job["variants"][variant]["source_sha256"] ==
                    first["variants"][variant]["source_sha256"],
                    f"incomparable {variant} source_sha256 across jobs")
        identity = _canonical_id
        require({identity(entry) for entry in job["samples"][0]["workloads"]} ==
                {identity(entry) for entry in first["samples"][0]["workloads"]},
                "incomparable workload identities across jobs")


def active_allowance(metric, baseline):
    """Find the integer allowance through the authoritative active policy."""
    low, high = 0, 1
    while within_limit(metric, baseline, baseline + high):
        low, high = high, high * 2
    while high - low > 1:
        middle = (low + high) // 2
        if within_limit(metric, baseline, baseline + middle):
            low = middle
        else:
            high = middle
    return low


def distribution(values):
    ordered = sorted(value for value in values if value is not None)
    return {
        "n": len(ordered),
        "undefined": len(values) - len(ordered),
        "min": ordered[0] if ordered else None,
        "max": ordered[-1] if ordered else None,
        **{f"p{percentile}": ordered[(percentile * len(ordered) + 99) // 100 - 1]
           if ordered else None for percentile in (50, 95)},
    }


def build_report(jobs):
    """Analyze already validated jobs; validation never participates in learning."""
    validate_comparability(jobs)
    comparisons = []
    for job in sorted(jobs, key=lambda job: job["job_id"]):
        pairs = defaultdict(dict)
        for sample in job["samples"]:
            pairs[sample["pair"]][sample["variant"]] = sample
        for pair, samples in sorted(pairs.items()):
            baseline = {_canonical_id(entry): entry["counts"]
                        for entry in samples["a"]["workloads"]}
            for variant in CONTROLS:
                for workload in samples[variant]["workloads"]:
                    identity = _canonical_id(workload)
                    for metric in METRICS:
                        before, after = baseline[identity][metric], workload["counts"][metric]
                        delta = after - before
                        comparisons.append({
                            "job_id": job["job_id"], "role": job["role"], "pair": pair,
                            "variant": variant, "function_name": identity[0], "id": identity[1],
                            "metric": metric, "baseline": before, "candidate": after,
                            "absolute_delta": delta,
                            "relative_delta": delta / before if before else None,
                            "relative_delta_exact": {"numerator": delta, "denominator": before}
                            if before else None,
                            "current_passed": within_limit(metric, before, after),
                        })
    floors = defaultdict(int)
    for row in comparisons:
        if row["role"] == "discovery" and row["variant"] == "b" and row["metric"] != "Ir":
            key = (row["function_name"], row["id"], row["metric"])
            floors[key] = max(floors[key], row["absolute_delta"])
    allowances = []
    identities = sorted({(row["function_name"], row["id"]) for row in comparisons},
                        key=lambda identity: (identity[0], identity[1] or ""))
    for name, identifier in identities:
        for metric in METRICS[1:]:
            allowances.append({"function_name": name, "id": identifier, "metric": metric,
                               "discovery_positive_delta_floor": floors[(name, identifier, metric)]})
    for row in comparisons:
        row["active_allowance"] = active_allowance(row["metric"], row["baseline"])
        row["proposed_allowance"] = max(row["active_allowance"], floors[
            (row["function_name"], row["id"], row["metric"])]) if row["metric"] != "Ir" else row["active_allowance"]
        row["proposed_passed"] = row["absolute_delta"] <= row["proposed_allowance"]
    groups = defaultdict(list)
    for row in comparisons:
        groups[(row["role"], row["variant"], row["function_name"], row["id"], row["metric"])].append(row)
    summaries = []
    for (role, variant, name, identifier, metric), rows in sorted(
            groups.items(), key=lambda item: tuple(value or "" for value in item[0])):
        exact_relative = [Fraction(row["absolute_delta"], row["baseline"])
                          if row["baseline"] else None for row in rows]
        relative = distribution(exact_relative)
        relative_exact = {key: {"numerator": value.numerator, "denominator": value.denominator}
                          if isinstance(value, Fraction) else value for key, value in relative.items()}
        summaries.append({
            "role": role, "variant": variant, "function_name": name, "id": identifier,
            "metric": metric, "samples": len(rows),
            "absolute_delta": distribution([row["absolute_delta"] for row in rows]),
            "relative_delta": {key: float(value) if isinstance(value, Fraction) else value
                               for key, value in relative.items()},
            "relative_delta_exact": relative_exact,
            "current_exceedances": sum(not row["current_passed"] for row in rows),
            "proposed_exceedances": sum(not row["proposed_passed"] for row in rows),
        })
    validation_aa = [row for row in comparisons if row["role"] == "validation" and row["variant"] == "b"]
    signals = [row for row in comparisons if row["variant"] in ("extra_work", "extra_alloc")
               and row["metric"] == "Ir" and (row["function_name"], row["id"]) in SIGNALS]
    failures = [row for row in signals if row["role"] == "validation" and row["current_passed"]]
    role_jobs = {role: [job for job in jobs if job["role"] == role] for role in ROLES}
    sufficient = (len(role_jobs["discovery"]) == 7 and len(role_jobs["validation"]) == 3 and
                  all(len(job["samples"]) == 25 for job in jobs))
    return {
        "format_version": 1, "mode": "proposal-only", "active_gates_changed": False,
        "coverage": {
            "planned_jobs": {"discovery": 7, "validation": 3}, "planned_pairs_per_job": 5,
            "observed_jobs": {role: len(entries) for role, entries in role_jobs.items()},
            "observed_pairs": {job["job_id"]: len(job["samples"]) // 5 for job in jobs},
            "predeclared_coverage_met": sufficient,
            "limitation": "Insufficient predeclared coverage. No calibrated policy approval or tail reliability claim."
            if not sufficient else "Predeclared coverage collected; finite dependent samples do not establish tail reliability or policy approval.",
        },
        "dependence": "Each pair reuses the same a baseline for b, layout, extra_work and extra_alloc. Comparisons within a pair are dependent; pairs within a job share builds and environment.",
        "learning": "Cache allowance per workload/event is max(active allowance for the comparison baseline, maximum positive discovery A/A delta). Validation, layout, and positive controls never train it. Ir remains unchanged at 2%.",
        "layout": "Diagnostic only; layout is not assumed cost-equivalent A/A noise and does not train allowances.",
        "percentiles": "Nearest-rank empirical p50/p95; not estimates of independent tail reliability.",
        "jobs": [{key: job[key] for key in ("job_id", "role", "source_revision", "environment",
                                            "fixture_sha256", "harness_sha256", "variants")}
                 | {"execution_order": [{key: sample[key] for key in ("pair", "variant", "order")}
                                        for sample in job["samples"]]} for job in jobs],
        "hosts": {job["job_id"]: job.get("host", {}) for job in jobs},
        "cache_allowances": allowances, "comparisons": comparisons, "summaries": summaries,
        "held_out_aa": {
            "samples": len(validation_aa),
            "current_false_positives": sum(not row["current_passed"] for row in validation_aa),
            "proposed_false_positives": sum(not row["proposed_passed"] for row in validation_aa),
            "by_metric": {metric: {
                "samples": sum(row["metric"] == metric for row in validation_aa),
                "current_false_positives": sum(row["metric"] == metric and not row["current_passed"] for row in validation_aa),
                "proposed_false_positives": sum(row["metric"] == metric and not row["proposed_passed"] for row in validation_aa),
            } for metric in METRICS},
        },
        "sensitivity": {"passed": not failures, "policy": "unchanged Ir 2%",
                        "signals": signals, "validation_failures": failures},
    }


def render_markdown(report):
    escape = lambda value: str(value).replace("|", "\\|").replace("\n", " ")
    lines = ["# Performance calibration (proposal-only)", "",
             "Active blocking gates are unchanged. This report does not approve a calibrated policy.", "",
             "## Coverage and dependence", "", report["coverage"]["limitation"], "",
             f"Observed jobs: {report['coverage']['observed_jobs']}; pairs: {report['coverage']['observed_pairs']}.", "",
             report["dependence"], "", report["percentiles"], "", "## Discovery-only proposal", "",
             report["learning"], "", report["layout"], "",
             "| Workload | Event | Discovery positive A/A floor (events) |",
             "| --- | --- | ---: |"]
    for row in report["cache_allowances"]:
        name = row["function_name"] + (f" [{row['id']}]" if row["id"] is not None else "")
        lines.append(f"| {escape(name)} | {row['metric']} | {row['discovery_positive_delta_floor']} |")
    lines += ["", "## Held-out A/A false positives", "",
              "Cache A/A noise is reported, never suppressed, and does not fail collection or this report.", "",
              "| Event | Comparisons | Current false positives | Proposed false positives |",
              "| --- | ---: | ---: | ---: |"]
    for metric, values in report["held_out_aa"]["by_metric"].items():
        lines.append(f"| {metric} | {values['samples']} | {values['current_false_positives']} | {values['proposed_false_positives']} |")
    lines += ["", "## Unchanged 2% Ir positive-control sensitivity", "",
              "PASS" if report["sensitivity"]["passed"] else "FAIL: one or more validation controls were not detected.", "",
              "| Job | Pair | Control | Workload | Ir delta | Ir relative delta | Detected |",
              "| --- | ---: | --- | --- | ---: | ---: | --- |"]
    for row in report["sensitivity"]["signals"]:
        relative = "undefined (zero baseline)" if row["relative_delta"] is None else f"{row['relative_delta']:+.2%}"
        lines.append(f"| {escape(row['job_id'])} | {row['pair']} | {row['variant']} | {escape(row['id'])} | {row['absolute_delta']:+d} | {relative} | {'no' if row['current_passed'] else 'yes'} |")
    lines += ["", "## All A/A and control distributions", "",
              "Relative deltas retain sign; zero baselines are undefined, but absolute comparisons remain active.", "",
              "| Role | Variant | Workload | Event | n | Delta min / p50 / p95 / max | Relative p50 / p95 / max | Undefined relative | Current / proposed exceedances |",
              "| --- | --- | --- | --- | ---: | --- | --- | ---: | --- |"]
    for row in report["summaries"]:
        absolute, relative = row["absolute_delta"], row["relative_delta"]
        name = row["function_name"] + (f" [{row['id']}]" if row["id"] is not None else "")
        deltas = " / ".join(str(absolute[key]) for key in ("min", "p50", "p95", "max"))
        percentages = " / ".join("undefined" if relative[key] is None else f"{relative[key]:+.2%}"
                                 for key in ("p50", "p95", "max"))
        lines.append(f"| {row['role']} | {row['variant']} | {escape(name)} | {row['metric']} | {row['samples']} | {deltas} | {percentages} | {relative['undefined']} | {row['current_exceedances']} / {row['proposed_exceedances']} |")
    return "\n".join(lines) + "\n"


def positive_integer(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("expected a positive integer")
    return number


def check_expected_coverage(jobs, discovery=None, validation=None, pairs=None):
    for role, expected in (("discovery", discovery), ("validation", validation)):
        if expected is not None:
            observed = sum(job["role"] == role for job in jobs)
            require(observed == expected, f"expected {expected} {role} jobs, found {observed}")
    if pairs is not None:
        for job in jobs:
            observed = len(job["samples"]) // len(VARIANTS)
            require(observed == pairs,
                    f"job {job['job_id']}: expected {pairs} complete pairs, found {observed}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input_root", type=Path)
    parser.add_argument("--json-output", required=True, type=Path)
    parser.add_argument("--markdown-output", required=True, type=Path)
    parser.add_argument("--expected-discovery-jobs", type=positive_integer)
    parser.add_argument("--expected-validation-jobs", type=positive_integer)
    parser.add_argument("--expected-pairs", type=positive_integer)
    args = parser.parse_args()
    try:
        jobs = read_datasets(args.input_root)
        check_expected_coverage(jobs, args.expected_discovery_jobs, args.expected_validation_jobs,
                                args.expected_pairs)
        report = build_report(jobs)
        for path, text in ((args.json_output, json.dumps(report, indent=2, allow_nan=False) + "\n"),
                           (args.markdown_output, render_markdown(report))):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
    except (ValueError, OSError) as error:
        print(f"Calibration data error: {error}", file=sys.stderr)
        return 1
    if not report["sensitivity"]["passed"]:
        print("Calibration sensitivity failed: validation work/alloc controls did not exceed unchanged 2% Ir policy.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
