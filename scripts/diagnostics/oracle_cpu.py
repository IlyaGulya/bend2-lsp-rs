#!/usr/bin/env python3
"""Hosted-only diagnostic; unchanged full oracle protocol and gate resource limits."""
import argparse
from collections import Counter, defaultdict
import gzip
import json
import os
from pathlib import Path
import shutil
import signal
import sys
import time
from urllib.parse import unquote, urlsplit
import zipfile

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))
import semantic_conformance as conformance
import semantic_gate as gate

REFERENCE_SOURCE = "a2eaa2255e5baa3e012e9796e4d1cd51bee72334"
REFERENCE_ZIP_SHA = "b8dac4fbc28fb8b3ed2448ad507586e772d8adf4339fa02ec3a63cb1a9284609"
SOURCE_FINGERPRINT = "a1189e29a0c150ceafe3a9d5b1e77d2639bb1b614f4801821171e5f25b681550"
INSTALLATION = "7bdd49a7f3815db40b49cb0007dc1ddad128911f2353ac7d188919122460a096"
RUNTIME_SHA = "33d56b070be6a9e3da0ab013038b43d1645d0534ca811ecdba4472599117eb4b"
REVISION = "34cbb9a40b4bd1bd767d134a7065e66c2432a676"
COUNTS = {"planned": 634, "compared": 627, "matched": 627, "intended_invalid": 5, "recovery": 2}
PREFLIGHT = r'''
import { Worker } from "node:worker_threads";
const message = Promise.withResolvers();
const online = Promise.withResolvers();
const worker = new Worker(`
  const { parentPort } = require("node:worker_threads");
  parentPort.on("message", () => {
    function preflightWorkerBurn() {
      const end = performance.now() + 300;
      let value = 1;
      while (performance.now() < end) value = Math.imul(value + 1, 17);
      return value;
    }
    parentPort.postMessage(preflightWorkerBurn());
  });
`, { eval: true });
worker.once("online", online.resolve);
worker.once("message", message.resolve);
worker.once("error", message.reject);
await online.promise;
worker.postMessage("start");
function preflightMainBurn() {
  const end = performance.now() + 300;
  let value = 1;
  while (performance.now() < end) value = Math.imul(value + 1, 17);
  return value;
}
preflightMainBurn();
await message.promise;
await worker.terminate();
console.log(JSON.stringify({ status: "complete", workerApi: typeof worker.startCpuProfile }));
process.kill(process.pid, "SIGTERM");
'''


def require(condition, message):
    if not condition:
        raise ValueError(message)


def profiles(directory, expected_pids=None, installation=None):
    events_path = directory / "capture-events.jsonl"
    require(events_path.is_file(), "no profiler events")
    events = [json.loads(line) for line in events_path.read_text().splitlines()]
    require(not any(event.get("event") == "error" for event in events), "profiler API/write/flush error; inspect events")
    started = {event["pid"] for event in events if event["event"] == "start"}
    finished = {event["pid"] for event in events if event["event"] == "finished"}
    require(started and started == finished, "a profiler owner did not flush within the original shutdown grace")
    if expected_pids is not None:
        require(started == set(expected_pids), "profile PID set differs from full forward/reverse/fresh histories")
    sampling = next(event["sampling"] for event in events if event["event"] == "start")
    for event in events:
        if event["event"] == "start":
            require(event["bun"] == "1.4.0" and event["revision"] == REVISION, "runtime version/revision drift")
            require(event["sampling"] == sampling, "profile owners used different sampling policies")
    counts = Counter()
    self_samples = defaultdict(Counter)
    inclusive_sources = defaultdict(Counter)
    seen_sources = defaultdict(set)
    inventory = []
    installed_sources = {str(path.relative_to(installation)) for path in installation.rglob("*")
        if path.is_file()} if installation is not None else set()
    url_sources = {}
    for event in events:
        if event["event"] != "profile":
            continue
        name = event["file"]
        require(Path(name).name == name and name.endswith(".cpuprofile.gz"), "invalid profile path")
        filename = directory / "profiles" / name
        require(filename.stat().st_size == event["bytes"], "profile size mismatch")
        require(conformance.file_digest(filename) == event["sha256"], "compressed profile digest mismatch")
        raw = gzip.decompress(filename.read_bytes())
        require(len(raw) == event["rawBytes"] and conformance.digest(raw) == event["rawSha256"], "raw profile digest mismatch")
        profile = json.loads(raw)
        samples, deltas = profile["samples"], profile["timeDeltas"]
        require(len(samples) == len(deltas) == event["samples"] and samples, "empty/malformed sample coverage")
        require(all(isinstance(delta, (int, float)) and delta >= 0 for delta in deltas), "invalid sample time deltas")
        nodes = {node["id"]: node for node in profile["nodes"]}
        parents = {}
        for node in nodes.values():
            for child in node.get("children", []):
                require(child not in parents, "CPU profile is not a tree")
                parents[child] = node["id"]
        # Resolve ancestry/source ownership once per node, not for every sample.
        node_sources = {}
        for node_id in nodes:
            pending, visited, current = [], set(), node_id
            while current is not None and current not in node_sources:
                require(current in nodes and current not in visited, "invalid CPU profile ancestry")
                visited.add(current)
                pending.append(current)
                current = parents.get(current)
            inherited = frozenset() if current is None else node_sources[current]
            for current in reversed(pending):
                url = nodes[current]["callFrame"].get("url", "")
                if url not in url_sources:
                    value = unquote(urlsplit(url).path) if url.startswith("file:") else url
                    relative = None
                    if installation is not None and value.startswith(str(installation) + "/"):
                        relative = str(Path(value).relative_to(installation))
                        require(relative in installed_sources, "profile source absent from authenticated installation")
                    url_sources[url] = relative
                source = url_sources[url]
                if source is not None:
                    inherited = inherited | {source}
                node_sources[current] = inherited
        key = (event["pid"], event["role"])
        counts[key] += len(samples)
        for sample, delta in zip(samples, deltas, strict=True):
            require(sample in nodes, "sample refers to an absent node")
            frame = nodes[sample]["callFrame"]
            self_samples[key][(frame.get("url", ""), frame.get("functionName", ""))] += delta
            seen_sources[key].update(node_sources[sample])
            for source in node_sources[sample]:
                inclusive_sources[key][source] += delta
        inventory.append(event)
    require(counts and all(counts[(pid, role)] > 0 for pid in started for role in ("main", "pristine")), "missing main/pristine samples for an oracle owner")
    if installation is not None:
        for pid in started:
            require("instrumented.ts" in seen_sources[(pid, "main")], "missing instrumented main source frames")
            require("bend.ts" in seen_sources[(pid, "pristine")], "missing pristine compiler source frames")
        for role in ("main", "pristine"):
            require(any("semantic_book_stream.ts" in names for (pid, found_role), names in seen_sources.items() if found_role == role), "missing full-Book canonical stream source frames")
    attribution = []
    for key, rows in sorted(self_samples.items()):
        attribution.append({"pid": key[0], "role": key[1], "samples": counts[key],
            "top_self_sample_microseconds": [{"url": item[0], "function": item[1], "microseconds": value} for item, value in rows.most_common(50)],
            "inclusive_source_microseconds_nonadditive": dict(inclusive_sources[key]),
            "installed_sources_seen": sorted(seen_sources[key])})
    return {"profiles": inventory, "attribution": attribution,
        "sampling_interval_microseconds": sampling["intervalMicroseconds"],
        "active_window_milliseconds": sampling["activeMilliseconds"],
        "pause_milliseconds": sampling["pauseMilliseconds"],
        "worker_module_startup_covered": False,
        "sampling_gaps": "explicit pauses recorded by each owner, plus stop/serialize/compress/restart; not continuous sampling",
        "scope": "weighted JS samples, including profiler perturbation; inclusive source families overlap; not phase wall time or admission"}


def worker(args):
    plan = json.loads(args.plan.read_text())
    original = conformance.native_worker_command

    def command(runtime, installation):
        argv = original(runtime, installation)
        return [argv[0], "--preload=" + str(args.preload), *argv[1:]]

    conformance.native_worker_command = command
    report = conformance.execute(plan, str(gate.default_candidate()), args.output / "failures",
        runtime=conformance.runtime_path(), deadline_seconds=args.deadline_seconds,
        lsp=str(gate.default_candidate().parent / "bend2-lsp"))
    gate.write_json(args.output / "worker-report.json", report)
    print(json.dumps({"status": report["status"], "counts": report["counts"]}))
    return 0 if report["status"] == "pass" else 1


def capture(args):
    require(gate.source_identity() == SOURCE_FINGERPRINT, "accepted Rust source fingerprint drift")
    require(conformance.file_digest(args.reference) == REFERENCE_ZIP_SHA, "reference gate ZIP digest mismatch")
    with zipfile.ZipFile(args.reference) as archive:
        require(archive.testzip() is None, "reference ZIP CRC mismatch")
        reference_gate = json.loads(archive.read("attempt-mzr9et6_/gate-report.json"))
        reference = json.loads(archive.read("attempt-mzr9et6_/worker-report.json"))
    require(reference_gate["environment"]["ci"]["GITHUB_SHA"] == REFERENCE_SOURCE, "reference source mismatch")
    runtime = conformance.runtime_path()
    installation, provenance = conformance.install(runtime, check=True)
    require(provenance == reference["provenance"] and provenance["installation"] == INSTALLATION, "accepted installation/provenance drift")
    require(conformance.file_digest(runtime) == RUNTIME_SHA, "hosted runtime executable digest drift")
    installed_before = {path.name: conformance.file_digest(path) for path in installation.iterdir() if path.is_file()}
    output = args.output.resolve()
    require(not output.exists(), "capture output must be fresh")
    output.mkdir(parents=True)
    receipt = {"diagnostic_only": True, "performance_admission": False,
        "accepted_core_source": REFERENCE_SOURCE, "source_fingerprint": SOURCE_FINGERPRINT,
        "provenance": provenance, "runtime_sha256": RUNTIME_SHA,
        "driver_sha256": conformance.file_digest(__file__),
        "preload_sha256": conformance.file_digest(Path(__file__).with_suffix(".ts")),
        "environment": gate.environment(), "reference_gate_zip_sha256": REFERENCE_ZIP_SHA,
        "original_resource_limits": gate.PROFILE, "stages": {}, "status": "incomplete"}
    try:
        preflight = output / "preflight"
        preflight.mkdir()
        shutil.copyfile(Path(__file__).with_suffix(".ts"), preflight / "oracle_cpu.ts")
        (preflight / "driver.ts").write_text(PREFLIGHT)
        stage = gate.supervise([runtime, "--preload=" + str(preflight / "oracle_cpu.ts"), str(preflight / "driver.ts")],
            deadline=time.monotonic() + 15, output_path=preflight / "worker.log", artifact_dir=preflight)
        receipt["stages"]["preflight"] = stage
        require(stage["status"] == "pass", "dual-thread capability smoke failed: " + str(stage["error"]))
        receipt["preflight"] = profiles(preflight)
        full = output / "full"
        full.mkdir()
        preload = full / "oracle_cpu.ts"
        shutil.copyfile(Path(__file__).with_suffix(".ts"), preload)
        began = time.monotonic()
        deadline = began + gate.PROFILE["hard_seconds"]
        plan = conformance.make_plan("pr", gate.PROFILE["seed"], gate.PROFILE["generated_projects"])
        for field in ("manifest_sha256", "pin_sha256", "profile_sha256", "corpus_sha256"):
            require(plan[field] == reference[field], "plan identity drift: " + field)
        plan_path = full / "plan.json"
        gate.write_json(plan_path, plan)
        stage = gate.supervise([sys.executable, str(Path(__file__).resolve()), "--worker", "--plan", str(plan_path),
            "--preload", str(preload), "--output", str(full), "--deadline-seconds",
            str(max(0.001, deadline - time.monotonic() - gate.PROFILE["shutdown_seconds"]))],
            deadline=deadline, output_path=full / "worker.log", artifact_dir=full)
        receipt["stages"]["full"] = stage
        require(stage["status"] == "pass", "complete original-budget capture failed: " + str(stage["error"]))
        report = gate.bounded_json(full / "worker-report.json", gate.PROFILE["report_bytes"])
        gate.validate_report(plan, report)
        require(all(report["counts"][key] == value for key, value in COUNTS.items()), "incomplete corpus")
        require(report["harness_sha256"] == reference["harness_sha256"], "accepted harness bytes changed")
        require(report["candidate_source_sha256"] == reference["candidate_source_sha256"], "candidate source changed")
        require(report["provenance"] == provenance, "oracle provenance changed")
        expected_rows = [(row["id"], row["input_sha256"], row.get("oracle_sha256")) for row in reference["completed"]]
        actual_rows = [(row["id"], row["input_sha256"], row.get("oracle_sha256")) for row in report["completed"]]
        require(actual_rows == expected_rows, "forward oracle semantic fingerprints changed under profiling")
        require(report["isolation"] == reference["isolation"], "reverse/ABA/fresh/repeated semantic fingerprints changed")
        require(report["stdio"] == reference["stdio"], "real stdio consumer observations changed")
        histories = report["metrics"]["histories"]
        pids = [histories["forward"]["pid"], histories["reverse"]["pid"], histories["fresh_pid"]]
        receipt["full_profile"] = profiles(full, pids, installation)
        require(time.monotonic() <= deadline, "capture validation exceeded original hard deadline")
        installed_after = {path.name: conformance.file_digest(path) for path in installation.iterdir() if path.is_file()}
        require(installed_before == installed_after, "accepted installation bytes changed")
        require(conformance.install(runtime, check=True)[1] == provenance, "post-capture installation authentication failed")
        receipt.update(status="complete", counts=report["counts"],
            forward_and_reverse_fingerprints_equal_reference=True,
            isolation_and_stdio_equal_reference=True, installed_files_sha256=installed_after,
            candidate_sha256=report["candidate_sha256"], lsp_sha256=report["lsp_sha256"],
            original_oracle_argv=conformance.native_worker_command(runtime, installation),
            diagnostic_oracle_argv=[runtime, "--preload=" + str(preload), str(installation / "worker.ts")])
        return 0
    except (OSError, ValueError, KeyError, TypeError, RuntimeError) as error:
        receipt["error"] = str(error)
        print("diagnostic capture failed: " + str(error), file=sys.stderr)
        return 1
    finally:
        gate.write_json(output / "capture-manifest.json", receipt)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reference", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--worker", action="store_true")
    parser.add_argument("--plan", type=Path)
    parser.add_argument("--preload", type=Path)
    parser.add_argument("--deadline-seconds", type=float)
    args = parser.parse_args()
    require(sys.platform == "linux", "hosted Linux x64 capture only")
    require(os.environ.get("GITHUB_ACTIONS") == "true", "local performance/profile capture is forbidden")
    signal.signal(signal.SIGTERM, gate.handle_shutdown_signal)
    signal.signal(signal.SIGINT, gate.handle_shutdown_signal)
    return worker(args) if args.worker else capture(args)


if __name__ == "__main__":
    sys.exit(main())
