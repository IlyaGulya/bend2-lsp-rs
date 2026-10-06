"""Disposable fixed reproducibility matrix; no acceptance comparator or retries.

Two independently built main ELF files, three memo ELF files, one previous ELF.
Each of eight unchanged failing workloads runs three times per ELF (144 samples).
All original sources/ELFs, failures, commands and exclusive counters are retained.
"""

import argparse
from collections import defaultdict
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess

from performance_calibration import parse_executable, run_logged, sha256, source_manifest

REVISIONS = {
    "main": "1e75117a8d5be38d648f65f25d854c894cffbafd",
    "memo": "4c9f598d34bee3c2e0e9306dd7ad3c56f2f868ae",
    "previous": "b642de8a8f473199a07000faf5578e68246d7b40",
}
BUILDS = (("main_a", "main"), ("memo_a", "memo"), ("previous", "previous"),
          ("memo_b", "memo"), ("main_b", "main"), ("memo_c", "memo"))
WORKLOADS = (
    ("completion_warm", "small", 4, 0),
    ("inlay_hints_warm", "small", 8, 0),
    ("call_hierarchy_warm", "medium", 7, 1),
    ("references_warm", "medium", 6, 1),
    ("workspace_initial_build", "hundred_files", 16, 0),
    ("workspace_incremental_invalidation", "hundred_files", 17, 0),
    ("workspace_burst_revision_invalidation", "sixteen_revisions", 19, 0),
    ("workspace_references", "hundred_files", 18, 0),
)
EVENTS = "Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw".split()


def raw_profile(path):
    functions = defaultdict(lambda: [0] * 9)
    instructions = defaultdict(lambda: [0] * 9)
    current = ""
    call_cost = False
    positions = events = summary = None
    metadata = []
    for row in path.read_text().splitlines():
        if row.startswith("positions:"):
            positions = row.split(":", 1)[1].split()
        elif row.startswith("events:"):
            events = row.split(":", 1)[1].split()
        elif row.startswith("summary:"):
            summary = [int(value) for value in row.split(":", 1)[1].split()]
            summary += [0] * (9 - len(summary))
        elif row.startswith(("creator:", "cmd:", "desc:")):
            metadata.append(row)
        elif row.startswith("fn="):
            current = row[3:]
            call_cost = False
        elif row.startswith("calls="):
            call_cost = True
        elif row and row[0].isdigit():
            if call_cost:
                call_cost = False
                continue
            if not current or not positions:
                raise ValueError("Missing exclusive row metadata")
            fields = row.split()
            values = [int(value) for value in fields[len(positions):]]
            values += [0] * (9 - len(values))
            if len(values) != 9:
                raise ValueError("Invalid event count")
            address = fields[positions.index("instr")] if "instr" in positions else "unknown"
            for index, value in enumerate(values):
                functions[current][index] += value
                instructions[current, address][index] += value
        elif row and row[0] in "+-*":
            raise ValueError("Unexpected compressed positions")
    if events != EVENTS or not positions or summary is None or len(summary) != 9:
        raise ValueError("Invalid raw event contract")
    exclusive = [sum(values[index] for values in functions.values()) for index in range(9)]
    if exclusive != summary:
        raise ValueError(f"Exclusive reconciliation failed: {exclusive} != {summary}")
    return {"events": dict(zip(EVENTS, summary)), "positions": positions,
            "metadata": metadata, "exclusive": dict(functions),
            "instructions": [{"function": fn, "address": address, "events": values}
                             for (fn, address), values in sorted(instructions.items())]}


def collect(source, work, output):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("Native Linux x86_64 required; no ARM/emulation substitution")
    if work.exists() or output.exists() or work.is_relative_to(output) or output.is_relative_to(work):
        raise ValueError("Use new disjoint directories; never overwrite a sample")
    work.mkdir(parents=True)
    output.mkdir(parents=True)
    environment = dict(os.environ)
    dataset = {"diagnostic_only": True, "acceptance_comparator_run": False,
               "revisions": REVISIONS, "fixed_builds": BUILDS, "fixed_workloads": WORKLOADS,
               "repetitions": 3, "planned_samples": 144, "builds": {}, "samples": [], "errors": [],
               "cache_state": "unchanged benchmark setup simulated before exact entry",
               "historical_acceptance_ELF": False, "ASLR": "disabled; fixed geometry"}

    def checkpoint():
        (output / "dataset.json").write_text(json.dumps(dataset, indent=2) + "\n")

    def execute(argv, label, cwd=source):
        return run_logged(argv, cwd, output / f"{label}.stdout", output / f"{label}.stderr", environment)

    checkpoint()
    try:
        dataset["environment"] = {
            "rustc": execute(["rustc", "-Vv"], "environment/rustc").strip(),
            "valgrind": execute(["valgrind", "--version"], "environment/valgrind").strip(),
            "kernel": execute(["uname", "-a"], "environment/kernel").strip(),
            "os_release": execute(["cat", "/etc/os-release"], "environment/os-release").strip(),
            "glibc": execute(["ldd", "--version"], "environment/glibc").strip(),
            "flags": {key: value for key, value in environment.items()
                      if key in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"}
                      or key.startswith("CARGO_PROFILE_")},
        }
        release = dict(line.split("=", 1) for line in dataset["environment"]["os_release"].splitlines() if "=" in line)
        if (not dataset["environment"]["rustc"].startswith("rustc 1.98.1 ")
                or dataset["environment"]["valgrind"] != "valgrind-3.22.0"
                or release.get("ID", "").strip('"') != "ubuntu"
                or release.get("VERSION_ID", "").strip('"') != "24.04"):
            raise ValueError("Requires Ubuntu24.04/Rust1.98.1/Valgrind3.22.0")
        execute(["setarch", "x86_64", "-R", "true"], "environment/aslr")
        checkouts, manifests = {}, {}
        for role, revision in REVISIONS.items():
            checkout = work / role
            checkout.mkdir()
            archive = output / f"{role}.tar"
            execute(["git", "archive", "--format=tar", f"--output={archive}", revision], f"archives/{role}")
            execute(["tar", "-xf", str(archive), "-C", str(checkout)], f"archives/{role}-extract")
            manifest = source_manifest(checkout)
            (output / f"{role}-source-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
            dataset.setdefault("archives", {})[role] = {"revision": revision, "sha256": sha256(archive)}
            checkouts[role], manifests[role] = checkout, manifest
        for variant, role in BUILDS:
            checkout = checkouts[role]
            target = work / f"target-{variant}"
            build = execute(["cargo", "bench", "--locked", "--bench", "analysis", "--no-run",
                             "--message-format=json", "--target-dir", str(target)], f"builds/{variant}/cargo", checkout)
            executable = parse_executable(build, checkout, target)
            if source_manifest(checkout) != manifests[role]:
                raise ValueError("Exact source changed during build")
            retained = output / "builds" / variant / "analysis.elf"
            shutil.copy2(executable, retained)
            nm = execute(["nm", "-C", "-S", "--defined-only", str(executable)], f"builds/{variant}/nm", checkout)
            execute(["objdump", "-d", "-C", "--no-show-raw-insn", str(executable)], f"builds/{variant}/disassembly", checkout)
            execute(["readelf", "-SW", str(executable)], f"builds/{variant}/sections", checkout)
            text = output / "builds" / variant / "text.bin"
            transformed = output / "builds" / variant / "objcopy-output.elf"
            execute(["objcopy", "--dump-section", f".text={text}", str(executable), str(transformed)], f"builds/{variant}/text-extract", checkout)
            transformed.unlink()
            if sha256(executable) != sha256(retained):
                raise ValueError("ELF capture mutated input")
            entries = {}
            for name, _, _, _ in WORKLOADS:
                entry = f"analysis::{name}::__iai_callgrind_wrapper_mod::{name}"
                matches = [line for line in nm.splitlines() if line.endswith(" " + entry)]
                if len(matches) != 1:
                    raise ValueError(f"Expected one physical entry: {entry}")
                row = re.fullmatch(r"([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+[tT]\s+(.+)", matches[0])
                if not row:
                    raise ValueError("Invalid nm entry")
                entries[name] = {"symbol": entry, "VMA": int(row[1], 16), "bytes": int(row[2], 16)}
            dataset["builds"][variant] = {
                "role": role, "source": manifests[role]["sha256"], "executable": str(executable),
                "retained_elf": str(retained.relative_to(output)), "binary_sha256": sha256(retained),
                "text_sha256": sha256(text), "text_bytes": text.stat().st_size, "entries": entries,
            }
            checkpoint()
        dataset["same_source_builds"] = {
            role: {"variants": [variant for variant, own_role in BUILDS if own_role == role],
                   "ELF_hashes": sorted({data["binary_sha256"] for data in dataset["builds"].values() if data["role"] == role}),
                   "text_hashes": sorted({data["text_sha256"] for data in dataset["builds"].values() if data["role"] == role})}
            for role in REVISIONS}
        variants = [variant for variant, _ in BUILDS]
        for repeat in range(3):
            for workload_index, (name, case, group, case_index) in enumerate(WORKLOADS):
                offset = (repeat + workload_index) % len(variants)
                order = variants[offset:] + variants[:offset]
                for variant in order:
                    build = dataset["builds"][variant]
                    checkout = checkouts[build["role"]]
                    sample_name = f"{name}.{case}-{variant}-r{repeat + 1}"
                    directory = output / "profiles" / sample_name
                    directory.mkdir(parents=True)
                    raw, log = directory / "callgrind.out", directory / "callgrind.log"
                    entry = build["entries"][name]["symbol"]
                    argv = ["setarch", "x86_64", "-R", "valgrind", "--tool=callgrind", "--cache-sim=yes",
                            "--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64",
                            "--collect-atstart=no", f"--toggle-collect={entry}", "--compress-pos=no",
                            "--compress-strings=no", "--dump-line=yes", "--dump-instr=yes", "--combine-dumps=no",
                            "--separate-threads=no", "--trace-children=yes", "--fair-sched=try", "--error-exitcode=200",
                            f"--callgrind-out-file={raw}", f"--log-file={log}", build["executable"],
                            "--iai-run", "analysis_hot_paths", str(group), str(case_index), f"analysis::analysis_hot_paths::{name}"]
                    sample = {"variant": variant, "workload": f"{name}.{case}", "repeat": repeat + 1,
                              "argv": argv, "scope_valid": False, "raw": str(raw.relative_to(output))}
                    try:
                        if (source_manifest(checkout) != manifests[build["role"]]
                                or sha256(Path(build["executable"])) != build["binary_sha256"]
                                or sha256(output / build["retained_elf"]) != build["binary_sha256"]):
                            raise ValueError("Frozen source/ELF changed before execution")
                        execute(argv, f"profiles/{sample_name}/process", checkout)
                        profile = raw_profile(raw)
                        (directory / "exclusive.json").write_text(json.dumps(profile, indent=2) + "\n")
                        if "instr" not in profile["positions"] or profile["exclusive"].get(entry, [0])[0] <= 0:
                            raise ValueError("Exact entry/instruction collection missing")
                        leaked = {fn: values[0] for fn, values in profile["exclusive"].items()
                                  if values[0] and ("::__run" in fn or "::__iai_callgrind_main" in fn)}
                        if leaked:
                            raise ValueError(f"Outer dispatch leaked: {leaked}")
                        if (source_manifest(checkout) != manifests[build["role"]]
                                or sha256(Path(build["executable"])) != build["binary_sha256"]):
                            raise ValueError("Frozen source/ELF changed during execution")
                        sample.update({"scope_valid": True, "events": profile["events"]})
                    except (ValueError, OSError, subprocess.CalledProcessError) as error:
                        sample["error"] = str(error)
                        dataset["errors"].append({"sample": sample_name, "error": str(error)})
                    dataset["samples"].append(sample)
                    checkpoint()
        if dataset["errors"] or len(dataset["samples"]) != 144:
            raise ValueError("Fixed matrix contains errors; all samples retained, no retry-to-green")
        print(json.dumps({"samples": len(dataset["samples"]), "same_source_builds": dataset["same_source_builds"]}, indent=2))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        dataset["fatal_error"] = str(error)
        checkpoint()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    collect(args.source.resolve(), args.work_dir.resolve(), args.output_dir.resolve())


if __name__ == "__main__":
    main()
