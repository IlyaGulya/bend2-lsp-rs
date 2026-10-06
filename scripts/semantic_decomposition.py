"""Disposable report-only semantic capability decomposition; no comparator/retries."""

import argparse
import hashlib
from collections import defaultdict
import json
import os
from pathlib import Path
import platform
import shutil

from performance_calibration import run_logged, sha256, source_manifest

BASELINE = "a575cf26efc4ae3296687ffebe3653b79ad50c01"
VARIANTS = (
    ("A", []),
    ("B", ["decomp-identity"]),
    ("C", ["decomp-occurrences"]),
    ("D", ["decomp-calls"]),
    ("E", ["decomp-occurrences", "decomp-calls"]),
    ("R", ["decomp-ref-consumer"]),
    ("F", ["decomp-ref-consumer", "decomp-call-consumer"]),
)
EVENTS = "Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw".split()
REPETITIONS = 3


def raw_profile(path):
    functions = defaultdict(lambda: [0] * 9)
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
            for index, value in enumerate(values):
                functions[current][index] += value
        elif row and row[0] in "+-*":
            raise ValueError("Unexpected compressed positions")
    if events != EVENTS or not positions or summary is None or len(summary) != 9:
        raise ValueError("Invalid raw event contract")
    exclusive = [sum(values[index] for values in functions.values()) for index in range(9)]
    if exclusive != summary:
        raise ValueError(f"Exclusive reconciliation failed: {exclusive} != {summary}")
    return {"events": dict(zip(EVENTS, summary)), "metadata": metadata,
            "exclusive": dict(functions)}


def cargo_executable(text, name, kind, target):
    artifacts = []
    finished = False
    for row in text.splitlines():
        message = json.loads(row)
        if message.get("reason") == "build-finished":
            if not message.get("success") or finished:
                raise ValueError("Cargo build did not finish successfully once")
            finished = True
        meta = message.get("target", {})
        if message.get("reason") == "compiler-artifact" and meta.get("name") == name and meta.get("kind") == [kind]:
            path = message.get("executable")
            if path:
                artifacts.append(Path(path).resolve())
    if not finished or len(artifacts) != 1:
        raise ValueError(f"Expected one successful {name} ELF; got {artifacts}")
    elf = artifacts[0]
    if not elf.is_relative_to(target.resolve()) or not elf.is_file():
        raise ValueError("ELF outside independent build target")
    return elf


def probe_evidence(path):
    probe = json.loads(path.read_text())
    digest = hashlib.sha256()
    for chunk in json.JSONEncoder(sort_keys=True, separators=(",", ":"), ensure_ascii=False).iterencode(probe["semantic_outputs"]):
        digest.update(chunk.encode())
    return {"path": str(path), "sha256": sha256(path), "semantic_outputs_sha256": digest.hexdigest(),
            "metadata": probe["metadata"]}


def collect(source, work, output):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("Native Linux x86_64 required; no ARM/emulation substitute")
    if work.exists() or output.exists() or work.is_relative_to(output) or output.is_relative_to(work):
        raise ValueError("New disjoint work/output directories required")
    work.mkdir(parents=True)
    output.mkdir(parents=True)
    environment = dict(os.environ)
    environment["PYTHONDONTWRITEBYTECODE"] = "1"
    dataset = {"report_only": True, "baseline": BASELINE, "acceptance_comparator_run": False,
               "variants": VARIANTS, "repetitions": REPETITIONS, "builds": {}, "samples": [],
               "errors": [], "ASLR": "disabled", "same_native_host": True,
               "cache_geometry": {"I1": "32768,8,64", "D1": "32768,8,64", "LL": "8388608,16,64"}}

    def checkpoint():
        (output / "dataset.json").write_text(json.dumps(dataset, indent=2) + "\n")

    def execute(argv, label, cwd=source, env=environment):
        dataset.setdefault("commands", []).append({"argv": argv, "cwd": str(cwd), "label": label})
        checkpoint()
        return run_logged(argv, cwd, output / f"{label}.stdout", output / f"{label}.stderr", env)

    checkpoint()
    try:
        dataset["environment"] = {
            "rustc": execute(["rustc", "-Vv"], "environment/rustc").strip(),
            "valgrind": execute(["valgrind", "--version"], "environment/valgrind").strip(),
            "kernel": execute(["uname", "-a"], "environment/kernel").strip(),
            "os_release": execute(["cat", "/etc/os-release"], "environment/os-release").strip(),
            "glibc": execute(["ldd", "--version"], "environment/glibc").strip(),
            "flags": {key: value for key, value in environment.items()
                      if key in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_INCREMENTAL"}
                      or key.startswith("CARGO_PROFILE_")},
        }
        if "release: 1.98.1" not in dataset["environment"]["rustc"] or "3.22.0" not in dataset["environment"]["valgrind"]:
            raise ValueError("Toolchain differs from preregistration")
        if 'VERSION_ID="24.04"' not in dataset["environment"]["os_release"]:
            raise ValueError("Ubuntu24.04 required")
        baseline = work / "baseline"
        baseline.mkdir()
        archive = output / "baseline.tar"
        execute(["git", "archive", "--format=tar", f"--output={archive}", BASELINE], "source/baseline-archive")
        execute(["tar", "-xf", str(archive), "-C", str(baseline)], "source/baseline-extract")
        # Identical diagnostic harness; production baseline src remains byte-for-byte unchanged.
        shutil.copy2(source / "benches/semantic_decomposition.rs", baseline / "benches/semantic_decomposition.rs")
        if (source / "benches/decomposition").exists():
            shutil.copytree(source / "benches/decomposition", baseline / "benches/decomposition")
        features = (source / "Cargo.toml").read_text().split("[features]\n", 1)[1].split("\n[", 1)[0]
        with (baseline / "Cargo.toml").open("a") as handle:
            handle.write("\n[features]\n" + features + "\n[[bench]]\nname = \"semantic_decomposition\"\nharness = false\n")
        dataset["source_manifests"] = {"A": source_manifest(baseline), "family": source_manifest(source)}
        workloads = json.loads((source / "benches/decomposition/workloads.json").read_text())
        dataset["workloads"] = workloads
        dataset["planned_samples"] = sum(len(row.get("applicable_variants", [variant for variant, _ in VARIANTS]))
                                         for row in workloads) * REPETITIONS
        dataset["absent_cells"] = [{"variant": variant, "workload": row["name"]}
                                  for row in workloads for variant, _ in VARIANTS
                                  if variant not in row.get("applicable_variants", [name for name, _ in VARIANTS])]
        elfs = {}
        probes = {}
        for variant, flags in VARIANTS:
            checkout = baseline if variant == "A" else source
            target = work / f"target-{variant}"
            env = {**environment, "CARGO_TARGET_DIR": str(target)}
            extra = ["--features", ",".join(flags)] if flags else []
            built = execute(["cargo", "bench", "--locked", "--bench", "semantic_decomposition", "--no-run",
                             "--message-format=json", *extra], f"builds/{variant}/build", checkout, env)
            elf = cargo_executable(built, "semantic_decomposition", "bench", target)
            saved = output / f"builds/{variant}/benchmark.elf"
            shutil.copy2(elf, saved)
            execute(["objcopy", "--only-section=.text", "-O", "binary", str(saved), str(saved.with_suffix(".text"))], f"builds/{variant}/objcopy")
            execute(["objdump", "-h", str(saved)], f"builds/{variant}/sections")
            execute(["nm", "-S", "-n", "-C", str(saved)], f"builds/{variant}/nm")
            execute(["objdump", "-d", "-C", str(saved)], f"builds/{variant}/disassembly")
            probe = output / f"builds/{variant}/probe.json"
            execute([str(elf), "--probe-output", str(probe)], f"builds/{variant}/probe", checkout, env)
            probes[variant] = probe_evidence(probe)
            elfs[variant] = (elf, checkout)
            dataset["builds"][variant] = {"elf": str(elf), "sha256": sha256(elf), "features": flags,
                                           "text_sha256": sha256(saved.with_suffix(".text")),
                                           "text_bytes": saved.with_suffix(".text").stat().st_size,
                                           "probe": probes[variant]}
            built_server = execute(["cargo", "build", "--locked", "--release", "--bin", "bend2-lsp",
                                    "--message-format=json", *extra], f"builds/{variant}/server-build", checkout, env)
            server = cargo_executable(built_server, "bend2-lsp", "bin", target)
            server_saved = output / f"builds/{variant}/server.elf"
            shutil.copy2(server, server_saved)
            execute(["objcopy", "--only-section=.text", "-O", "binary", str(server_saved),
                     str(server_saved.with_suffix(".text"))], f"builds/{variant}/server-objcopy")
            stdio = output / f"builds/{variant}/stdio.json"
            execute(["python3", str(source / "scripts/decomposition_stdio.py"), "--server", str(server),
                     "--work", str(work / f"stdio-{variant}"), "--output", str(stdio)],
                    f"builds/{variant}/stdio", checkout, env)
            dataset["builds"][variant]["server_sha256"] = sha256(server)
            dataset["builds"][variant]["server_text_bytes"] = server_saved.with_suffix(".text").stat().st_size
            dataset["builds"][variant]["stdio"] = json.loads(stdio.read_text())
            checkpoint()
        # Every declared consumer returns actual canonical results, not non-empty/count echoes.
        semantic_outputs = {variant: probe["semantic_outputs_sha256"] for variant, probe in probes.items()}
        if any(value != semantic_outputs["A"] for value in semantic_outputs.values()):
            raise ValueError("Semantic output equivalence failed; all probes retained")
        dataset["all_semantic_outputs_match"] = True
        if any(build["stdio"]["semantic_outputs"] != dataset["builds"]["A"]["stdio"]["semantic_outputs"]
               for build in dataset["builds"].values()):
            raise ValueError("Actual stdio consumer equivalence failed; outputs retained")
        dataset["all_stdio_outputs_match"] = True
        for number, workload in enumerate(workloads):
            for repetition in range(REPETITIONS):
                order = [(variant, flags) for variant, flags in VARIANTS
                         if variant in workload.get("applicable_variants", [name for name, _ in VARIANTS])]
                rotation = (number + repetition) % len(order)
                order = order[rotation:] + order[:rotation]
                for variant, _flags in order:
                    elf, checkout = elfs[variant]
                    label = f"profiles/{number:02d}-{workload['name']}/{variant}-{repetition}"
                    raw = output / f"{label}.out"
                    raw.parent.mkdir(parents=True, exist_ok=True)
                    entry = workload["entry"]
                    execute(["setarch", "x86_64", "-R", "valgrind", "--tool=callgrind", "--cache-sim=yes",
                             "--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64",
                             "--compress-strings=no", "--compress-pos=no", "--dump-instr=yes", "--collect-atstart=no",
                             f"--toggle-collect={entry}", f"--callgrind-out-file={raw}", str(elf),
                             "--iai-run", workload["group"], str(workload["function_index"]),
                             str(workload["case_index"]), workload["function"]], label, checkout)
                    parsed = raw_profile(raw)
                    if any(sum(values) for fn, values in parsed["exclusive"].items()
                           if fn.endswith("::__run") or fn.endswith("::__iai_callgrind_main")):
                        raise ValueError("Outer harness leaked into measured scope")
                    if not any(entry in fn and sum(values) for fn, values in parsed["exclusive"].items()):
                        raise ValueError("Exact entry has no exclusive work")
                    dataset["samples"].append({"variant": variant, "workload": workload["name"],
                                               "repeat": repetition, "raw": str(raw), **parsed})
                    checkpoint()
        for variant, (elf, _checkout) in elfs.items():
            if sha256(elf) != dataset["builds"][variant]["sha256"]:
                raise ValueError("Measured ELF changed")
        if source_manifest(source) != dataset["source_manifests"]["family"] or source_manifest(baseline) != dataset["source_manifests"]["A"]:
            raise ValueError("Source mutated during fixed experiment")
        dataset["complete"] = True
        checkpoint()
    except Exception as error:
        dataset["errors"].append({"type": type(error).__name__, "message": str(error)})
        checkpoint()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    collect(args.source.resolve(), args.work.resolve(), args.output.resolve())


if __name__ == "__main__":
    main()
