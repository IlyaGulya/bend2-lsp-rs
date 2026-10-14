#!/usr/bin/env python3
"""One declared cache-history experiment; report only, never admission."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile

METRICS = ("Ir", "I1mr", "ILmr")
TARGETS = (
    ("completion_warm", "large", "I1mr"),
    ("inlay_hints_warm", "large", "I1mr"),
    ("scoped_completion_warm", "small", "I1mr"),
    ("semantic_tokens_warm", "large", "I1mr"),
    ("import_completion_candidates_warm", "fuzzy", "ILmr"),
)
HELPER = '''
fn cache_history_control<T>(input: T) -> T {
    let reset = match std::env::var("BEND2_CACHE_CONTROL")
        .must_be("declared cache control mode")
        .as_str()
    {
        "keep" => false,
        "cold" => true,
        _ => panic!("undeclared cache control mode"),
    };
    if reset {
        iai_callgrind::client_requests::callgrind::stop_instrumentation();
        iai_callgrind::client_requests::callgrind::start_instrumentation();
    }
    input
}
'''
INLAY = '''fn inlay_hints_warm(fixture: WarmSnapshot) -> Vec<analysis::InlayHint> {
    std::hint::black_box(analysis::inlay_hints(
        std::hint::black_box(fixture.snapshot),
        analysis::TextRange::new(0, fixture.source.len()),
    ))
}'''


def sha(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def run(command, cwd, log, env=None):
    print("+", " ".join(map(str, command)), flush=True)
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("w") as output:
        result = subprocess.run(command, cwd=cwd, env=env, stdout=output,
                                stderr=subprocess.STDOUT, check=False)
    if result.returncode:
        print(log.read_text(), flush=True)
        raise RuntimeError(f"command exited {result.returncode}: {log}")


def checkout(repository, revision, destination):
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("source must be a full lowercase commit SHA")
    destination.mkdir(parents=True)
    archive = destination.parent / f"{destination.name}.tar"
    with archive.open("wb") as output:
        subprocess.run(["git", "archive", revision], cwd=repository,
                       stdout=output, check=True)
    with tarfile.open(archive) as source:
        source.extractall(destination, filter="data")
    archive.unlink()


def conditioned_harness(source):
    # Match the existing calibration convention: exact known setup anchors.
    # Only attributes (outside the collection guard) change, not query bodies.
    for name, _, _ in TARGETS:
        marker = f"\nfn {name}"
        if source.count(marker) != 1:
            raise ValueError(f"ambiguous workload {name}")
        end = source.index(marker)
        start = source.rfind("\n#[library_benchmark]\n", 0, end)
        if start < 0:
            raise ValueError(f"missing benchmark attributes for {name}")
        attributes = source[start:end]
        if name == "import_completion_candidates_warm":
            attributes, count = re.subn(
                r'(, )("[^"]+")(\)\])',
                r'\1cache_history_control(\2)\3', attributes)
            if count != 3:
                raise ValueError("import setup anchors changed")
        else:
            for size in ("small", "medium", "large"):
                expression = f"WarmSnapshot::{size}()"
                if name == "scoped_completion_warm":
                    expression = f"scoped_completion_case(&{expression})"
                if attributes.count(expression) != 1:
                    raise ValueError(f"setup anchor changed: {name}.{size}")
                attributes = attributes.replace(
                    expression, f"cache_history_control({expression})")
        source = source[:start] + attributes + source[end:]
    return source + HELPER


def positive_harness(source, variant):
    if source.count(INLAY) != 1:
        raise ValueError("positive control anchor changed")
    if variant == "extra_work":
        body = INLAY.split("{\n", 1)[1].rsplit("\n}", 1)[0]
        extra = body + ";\n"
    elif variant == "extra_alloc":
        extra = "    let _allocation = std::hint::black_box(vec![1u64; 65_536]);\n"
    else:
        raise ValueError("undeclared positive control")
    return source.replace(INLAY, INLAY.replace("{\n", "{\n" + extra, 1))


def build(source, target, output, label, controlled=False):
    log = output / "builds" / f"{label}.jsonl"
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith("IAI_CALLGRIND_") and key != "CARGO_TARGET_DIR"}
    environment["CARGO_TARGET_DIR"] = str(target)
    command = ["cargo", "bench", "--locked", "--bench", "analysis", "--no-run",
               "--message-format=json"]
    if controlled:
        command += ["--features", "iai-callgrind/client_requests"]
    run(command, source, log, environment)
    executables = []
    for line in log.read_text().splitlines():
        if not line.startswith("{"):
            continue
        item = json.loads(line)
        if item.get("reason") == "compiler-artifact" and item.get("executable"):
            if item["target"]["name"] == "analysis" and "bench" in item["target"]["kind"]:
                executables.append(Path(item["executable"]))
    if len(executables) != 1:
        raise ValueError(f"expected one benchmark executable: {executables}")
    binary = output / "binaries" / label / "analysis"
    binary.parent.mkdir(parents=True)
    shutil.copy2(executables[0], binary)
    return binary


def collect(binary, source, output, label, mode):
    home = output / "raw" / label
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith("IAI_CALLGRIND_") and key != "CARGO_TARGET_DIR"}
    environment["BEND2_CACHE_CONTROL"] = mode
    before = sha(binary)
    run([str(binary), "--output-format=json", "--save-summary=pretty-json",
         f"--save-baseline={label}", f"--home={home}",
         "--callgrind-args=--cache-sim=yes"], source,
        output / "logs" / f"{label}.log", environment)
    if sha(binary) != before:
        raise ValueError("benchmark executable changed during measurement")
    metrics = {}
    for path in sorted(home.rglob("summary.json")):
        summary = json.loads(path.read_text())
        profiles = [p for p in summary["profiles"] if p["tool"] == "Callgrind"]
        if len(profiles) != 1:
            raise ValueError("expected one Callgrind profile")
        events = profiles[0]["summaries"]["total"]["summary"]["Callgrind"]
        counts = {}
        for metric in METRICS:
            value = events[metric]["metrics"]
            if set(value) != {"Left"} or set(value["Left"]) != {"Int"}:
                raise ValueError("expected fresh integer metric, not a reused baseline")
            counts[metric] = value["Left"]["Int"]
        key = f'{summary["function_name"]}[{summary["id"]}]'
        if key in metrics or counts["Ir"] <= 0:
            raise ValueError(f"duplicate or unmeasured workload: {key}")
        metrics[key] = counts
        if any(summary["function_name"] == name and summary["id"] == case
               for name, case, _ in TARGETS):
            for index, raw in enumerate(profiles[0]["out_paths"]):
                run(["callgrind_annotate", "--auto=no", "--threshold=100",
                     "--show=Ir,I1mr,ILmr", raw], source,
                    output / "annotations" / label / f"{key}-{index}.txt")
    if not metrics:
        raise ValueError("no profiler measurements collected")
    return {"binary_sha256": before, "mode": mode, "metrics": metrics}


def allowance(baseline, metric):
    if metric == "Ir":
        return baseline * 2 // 100
    return max((baseline * 3 + 99) // 100, 3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, required=True)
    parser.add_argument("--base", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError("this declared control requires the hosted Linux CI environment")
    output, work = args.output.resolve(), args.work.resolve()
    if output.exists() or work.exists():
        raise ValueError("fresh experiment directories required")
    output.mkdir(parents=True)
    work.mkdir(parents=True)
    (output / "harnesses").mkdir()
    shutil.copy2(Path(__file__), output / "experiment.py")
    records, identities = {}, {}
    for label, revision in (("base", args.base), ("candidate", args.candidate)):
        source = work / label
        checkout(args.repository, revision, source)
        harness = source / "benches" / "analysis.rs"
        original = harness.read_text()
        identities[label] = {"revision": revision, "original_harness_sha256": sha(harness),
                             "lockfile_sha256": sha(source / "Cargo.lock")}
        shutil.copy2(harness, output / "harnesses" / f"{label}-original.rs")
        (output / "identities.json").write_text(json.dumps(identities, indent=2) + "\n")
        binary = build(source, work / "target", output, f"{label}-original")
        records[f"{label}-original"] = collect(binary, source, output, f"{label}-original", "keep")
        (output / "records.json").write_text(json.dumps(records, indent=2) + "\n")
        controlled = conditioned_harness(original)
        harness.write_text(controlled)
        identities[label]["controlled_harness_sha256"] = sha(harness)
        shutil.copy2(harness, output / "harnesses" / f"{label}-controlled.rs")
        (output / "identities.json").write_text(json.dumps(identities, indent=2) + "\n")
        binary = build(source, work / "target", output, f"{label}-controlled", controlled=True)
        # Fixed order, same executable. No re-run of a failed sample.
        for phase, mode in (("keep-a", "keep"), ("cold", "cold"), ("keep-b", "keep")):
            sample = f"{label}-{phase}"
            records[sample] = collect(binary, source, output, sample, mode)
            (output / "records.json").write_text(json.dumps(records, indent=2) + "\n")
        if label == "candidate":
            for variant in ("extra_work", "extra_alloc"):
                harness.write_text(positive_harness(controlled, variant))
                shutil.copy2(harness, output / "harnesses" / f"{variant}.rs")
                binary = build(source, work / "target", output, variant, controlled=True)
                records[variant] = collect(binary, source, output, variant, "cold")
        (output / "records.json").write_text(json.dumps(records, indent=2) + "\n")
    identities_set = [set(record["metrics"]) for record in records.values()]
    if any(keys != identities_set[0] for keys in identities_set[1:]):
        raise ValueError("control changed workload identities")
    aa = {label: records[f"{label}-keep-a"]["metrics"] == records[f"{label}-keep-b"]["metrics"]
          for label in ("base", "candidate")}
    failures = {}
    for phase in ("original", "keep-a", "cold"):
        failures[phase] = []
        for key, baseline in records[f"base-{phase}"]["metrics"].items():
            candidate = records[f"candidate-{phase}"]["metrics"][key]
            for metric in METRICS:
                if candidate[metric] > baseline[metric] + allowance(baseline[metric], metric):
                    failures[phase].append({"workload": key, "metric": metric,
                                            "baseline": baseline[metric], "candidate": candidate[metric]})
    sensitivity = []
    for variant in ("extra_work", "extra_alloc"):
        for size in ("small", "medium", "large"):
            key = f"inlay_hints_warm[{size}]"
            baseline = records["candidate-cold"]["metrics"][key]["Ir"]
            actual = records[variant]["metrics"][key]["Ir"]
            sensitivity.append({"variant": variant, "workload": key, "baseline": baseline,
                                "candidate": actual, "detected": actual > baseline + allowance(baseline, "Ir")})
    report = {"report_only": True, "active_gates_changed": False,
              "identities": identities, "same_binary_aa_exact": aa,
              "workload_count": len(identities_set[0]), "failures": failures,
              "positive_sensitivity": sensitivity,
              "environment": {"rustc": subprocess.check_output(["rustc", "-vV"], text=True),
                              "valgrind": subprocess.check_output(["valgrind", "--version"], text=True)}}
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    inventory = [{"path": str(path.relative_to(output)), "size": path.stat().st_size,
                  "sha256": sha(path)} for path in sorted(output.rglob("*")) if path.is_file()]
    (output / "inventory.json").write_text(json.dumps(inventory, indent=2) + "\n")
    print(json.dumps(report, indent=2), flush=True)
    if not all(row["detected"] for row in sensitivity):
        raise RuntimeError("conditioned measurements lost positive-work/allocation sensitivity")


if __name__ == "__main__":
    main()
