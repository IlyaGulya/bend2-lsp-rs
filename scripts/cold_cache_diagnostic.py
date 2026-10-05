"""Temporary native x86 cold-cache diagnostic; never an acceptance comparator."""

import argparse
import json
import os
import platform
import re
import shutil
import subprocess
from collections import defaultdict
from pathlib import Path

from performance_calibration import parse_executable, run_logged, sha256, source_manifest

CONTROL = "bcaefa8882f70a963af596b3ea429bd90c731a79"
CANDIDATE = "b642de8a8f473199a07000faf5578e68246d7b40"
EVENTS = "Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw".split()
ORDER = (
    ("small", ("control", "candidate_a", "candidate_b")),
    ("medium", ("candidate_a", "candidate_b", "control")),
    ("large", ("candidate_b", "control", "candidate_a")),
)


def raw_profile(path):
    functions = defaultdict(lambda: [0] * len(EVENTS))
    current = ""
    call_cost = False
    positions = []
    events = []
    summary = []
    metadata = []
    for row in path.read_text().splitlines():
        if row.startswith("positions: "):
            positions = row.removeprefix("positions: ").split()
        elif row.startswith("events: "):
            events = row.removeprefix("events: ").split()
        elif row.startswith("summary: "):
            summary = [int(value) for value in row.removeprefix("summary: ").split()]
            summary += [0] * (len(EVENTS) - len(summary))
        elif row.startswith(("creator:", "cmd:", "desc:")):
            metadata.append(row)
        elif row.startswith("fn="):
            current = row.removeprefix("fn=")
            call_cost = False
        elif row.startswith("calls="):
            call_cost = True
        elif row and row[0].isdigit():
            if call_cost:
                call_cost = False
                continue
            values = [int(value) for value in row.split()[len(positions):]]
            values += [0] * (len(EVENTS) - len(values))
            if not current or len(values) != len(EVENTS):
                raise ValueError(f"Unsupported exclusive row: {row}")
            for index, value in enumerate(values):
                functions[current][index] += value
    if events != EVENTS or not positions or not summary:
        raise ValueError("Missing supported raw events/positions/summary")
    exclusive = [sum(values[index] for values in functions.values()) for index in range(len(EVENTS))]
    if exclusive != summary:
        raise ValueError(f"Exclusive totals differ from raw summary: {exclusive} != {summary}")
    return {"events": dict(zip(EVENTS, summary)), "positions": positions, "metadata": metadata, "exclusive": dict(functions)}


def collect(source, work, output):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("Requires native Linux x86_64; ARM is not hosted reproduction")
    if work.exists() or output.exists():
        raise ValueError("Work/output must be new directories; never overwrite a diagnostic sample")
    work.mkdir(parents=True)
    output.mkdir(parents=True)
    environment = dict(os.environ)
    dataset = {"diagnostic_only": True, "acceptance_comparator_run": False, "control": CONTROL, "candidate": CANDIDATE, "builds": {}, "samples": [], "errors": []}

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
            "flags": {key: environment[key] for key in environment if key in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"} or key.startswith("CARGO_PROFILE_")},
        }
        if not dataset["environment"]["rustc"].startswith("rustc 1.98.1 ") or dataset["environment"]["valgrind"] != "valgrind-3.22.0":
            raise ValueError("Requires hosted Rust1.98.1 and Valgrind3.22.0 pins")
        execute(["setarch", "x86_64", "-R", "true"], "environment/aslr")
        checkouts = {}
        manifests = {}
        for role, revision in (("control", CONTROL), ("candidate", CANDIDATE)):
            checkout = work / role
            checkout.mkdir()
            archive = output / f"{role}.tar"
            execute(["git", "archive", "--format=tar", f"--output={archive}", revision], f"archives/{role}")
            execute(["tar", "-xf", str(archive), "-C", str(checkout)], f"archives/{role}-extract")
            manifest = source_manifest(checkout)
            (output / f"{role}-source-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
            dataset.setdefault("archives", {})[role] = {"source": revision, "sha256": sha256(archive)}
            checkouts[role] = checkout
            manifests[role] = manifest
        for variant, role in (("control", "control"), ("candidate_a", "candidate"), ("candidate_b", "candidate")):
            checkout = checkouts[role]
            target = work / f"target-{variant}"
            build = execute(["cargo", "bench", "--locked", "--bench", "analysis", "--no-run", "--message-format=json", "--target-dir", str(target)], f"builds/{variant}/cargo", checkout)
            executable = parse_executable(build, checkout, target)
            if source_manifest(checkout) != manifests[role]:
                raise ValueError(f"Exact {role} source changed during build")
            retained = output / "builds" / variant / "analysis.elf"
            shutil.copy2(executable, retained)
            execute(["nm", "-C", "-S", "--defined-only", str(executable)], f"builds/{variant}/nm", checkout)
            execute(["objdump", "-d", "-C", "--no-show-raw-insn", str(executable)], f"builds/{variant}/disassembly", checkout)
            execute(["readelf", "-SW", str(executable)], f"builds/{variant}/sections", checkout)
            text = output / "builds" / variant / "text.bin"
            extracted_copy = output / "builds" / variant / "objcopy-output.elf"
            execute(["objcopy", "--dump-section", f".text={text}", str(executable), str(extracted_copy)], f"builds/{variant}/text-extract", checkout)
            extracted_copy.unlink()
            if sha256(executable) != sha256(retained):
                raise ValueError("ELF capture/extraction mutated the measured input")
            dataset["builds"][variant] = {"role": role, "source": manifests[role]["sha256"], "executable": str(executable), "retained_elf": str(retained.relative_to(output)), "binary_sha256": sha256(retained), "text_sha256": sha256(text), "text_bytes": text.stat().st_size}
            checkpoint()
        dataset["candidate_AA"] = {
            "same_source": dataset["builds"]["candidate_a"]["source"] == dataset["builds"]["candidate_b"]["source"],
            "same_elf": dataset["builds"]["candidate_a"]["binary_sha256"] == dataset["builds"]["candidate_b"]["binary_sha256"],
            "same_text": dataset["builds"]["candidate_a"]["text_sha256"] == dataset["builds"]["candidate_b"]["text_sha256"],
        }
        for index, (scale, variants) in enumerate(ORDER):
            for variant in variants:
                build = dataset["builds"][variant]
                checkout = checkouts[build["role"]]
                current_source = source_manifest(checkout)
                current_binary = sha256(Path(build["executable"]))
                if current_source != manifests[build["role"]] or current_binary != build["binary_sha256"]:
                    raise ValueError(f"Frozen {variant} changed: source={current_source['sha256']} expected={build['source']}; ELF={current_binary} expected={build['binary_sha256']}")
                name = f"cold_snapshot_build_{scale}"
                entry = f"analysis::{name}::__iai_callgrind_wrapper_mod::{name}"
                sample_dir = output / "profiles" / f"{scale}-{variant}"
                sample_dir.mkdir(parents=True)
                raw = sample_dir / "callgrind.out"
                log = sample_dir / "callgrind.log"
                command = ["setarch", "x86_64", "-R", "valgrind", "--tool=callgrind", "--cache-sim=yes", "--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64", "--collect-atstart=no", f"--toggle-collect={entry}", "--compress-pos=no", "--compress-strings=no", "--dump-line=yes", "--dump-instr=yes", "--combine-dumps=no", "--separate-threads=no", "--trace-children=yes", "--fair-sched=try", "--error-exitcode=200", f"--callgrind-out-file={raw}", f"--log-file={log}", build["executable"], "--iai-run", "analysis_hot_paths", str(index), "0", f"analysis::analysis_hot_paths::{name}"]
                sample = {"scale": scale, "variant": variant, "argv": command, "scope_valid": False}
                try:
                    execute(command, f"profiles/{scale}-{variant}/process", checkout)
                    profile = raw_profile(raw)
                    (sample_dir / "exclusive.json").write_text(json.dumps(profile, indent=2) + "\n")
                    sample["events"] = profile["events"]
                    drops = {symbol: values[0] for symbol, values in profile["exclusive"].items() if "drop" in symbol and "DocumentSnapshot" in symbol and values[0]}
                    if drops or "instr" not in profile["positions"] or not any("DocumentSnapshot>::new" in symbol and values[0] for symbol, values in profile["exclusive"].items()):
                        raise ValueError(f"Invalid cold scope/instruction evidence; returned snapshot drops={drops}")
                    sample["scope_valid"] = True
                except (ValueError, OSError, subprocess.CalledProcessError) as error:
                    sample["error"] = str(error)
                    dataset["errors"].append({"variant": variant, "scale": scale, "error": str(error)})
                dataset["samples"].append(sample)
                checkpoint()
        if dataset["errors"] or len(dataset["samples"]) != 9:
            raise ValueError("Diagnostic contains errors; retain every sample, never retry to green")
        print(json.dumps({"candidate_AA": dataset["candidate_AA"], "samples": [{"scale": sample["scale"], "variant": sample["variant"], "events": sample["events"]} for sample in dataset["samples"]]}, indent=2))
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
