#!/usr/bin/env python3
"""Real-compiler differential correctness; optional hosted-only paired micro-cost."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import semantic_conformance as conformance
import semantic_gate as gate
from semantic_install import compiler_environment, install, runtime_path


def qualify(args):
    if not args.correctness_only and (sys.platform != "linux" or os.environ.get("GITHUB_ACTIONS") != "true"):
        raise ValueError("performance measurements are hosted-only; use --correctness-only locally")
    output = args.output.resolve()
    if output.exists():
        raise ValueError("qualification output must be fresh")
    runtime = runtime_path()
    installation, provenance = install(runtime, check=True)
    plan = conformance.make_plan("pr", gate.PROFILE["seed"], gate.PROFILE["generated_projects"])
    requests = []
    for item in plan["planned"]:
        request = {"id": item["id"], "input_sha256": item["input_sha256"], "facet": item["facet"],
            "request": conformance.request_for(item)}
        for field in ("expected_error", "expected_rejection"):
            value = item["case"].get(field)
            if value is not None:
                request[field] = value
        requests.append(request)
    output.mkdir(parents=True)
    gate.write_json(output / "plan.json", plan)
    gate.write_json(output / "requests.json", requests)
    receipt = {"schema": 1, "status": "incomplete", "performance_admission": False,
        "source": os.environ.get("GITHUB_SHA"), "source_fingerprint": gate.source_identity(),
        "candidate_source_sha256": conformance.candidate_source_digest(),
        "harness_sha256": conformance.digest(conformance.canonical(conformance.harness_sources())),
        "provenance": provenance, "plan_sha256": plan["manifest_sha256"],
        "corpus_sha256": plan["corpus_sha256"], "planned_views_per_compiler": len(requests),
        "mode": "correctness-only" if args.correctness_only else "hosted-micro-cost-and-correctness",
        "driver_sha256": conformance.file_digest(__file__),
        "typescript_sha256": conformance.file_digest(Path(__file__).with_suffix(".ts")),
        "normal_resource_limits_unchanged": gate.PROFILE,
        "scope": "Additional representation differential test, not a replacement for normal conformance, full histories, quality, p95 or strict Callgrind."}
    try:
        with (output / "worker.log").open("w") as log:
            result = subprocess.run([runtime, str(Path(__file__).with_suffix(".ts")), str(installation),
                str(output / "requests.json"), str(output), "correctness" if args.correctness_only else "hosted"],
                cwd=ROOT, env=compiler_environment(), stdout=log, stderr=subprocess.STDOUT, timeout=600)
        if result.returncode != 0:
            raise ValueError("native differential failed; inspect worker.log")
        report = gate.bounded_json(output / "corpus-equivalence.json", gate.PROFILE["report_bytes"])
        if report["status"] != "pass" or [entry["module"] for entry in report["modules"]] != ["bend.ts", "instrumented.ts"]:
            raise ValueError("incomplete compiler-module qualification")
        expected = [(item["id"], item["input_sha256"]) for item in requests]
        for entry in report["modules"]:
            if [(row["id"], row["input_sha256"]) for row in entry["completed"]] != expected:
                raise ValueError("missing/reordered corpus observations: " + entry["module"])
        if report["modules"][0]["completed"] != report["modules"][1]["completed"]:
            raise ValueError("plain/instrumented canonical observations differ")
        receipt.update(status="pass", all_planned_views_verified_for_both_compilers=True,
            plain_and_instrumented_observations_equal=True,
            corpus_equivalence_sha256=conformance.file_digest(output / "corpus-equivalence.json"))
        return 0
    except (OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        receipt.update(status="error", error=str(error))
        print("canonical qualification failed: " + str(error), file=sys.stderr)
        return 1
    finally:
        gate.write_json(output / "qualification.json", receipt)
        print(json.dumps({"status": receipt["status"], "output": str(output), "performance_admission": False}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--correctness-only", action="store_true", help="Run real corpus equivalence without local performance measurement")
    return qualify(parser.parse_args())


if __name__ == "__main__":
    sys.exit(main())
