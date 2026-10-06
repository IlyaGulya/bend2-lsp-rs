"""Disposable native allocator request tracing; no builds or acceptance comparison.

python3 scripts/warm_allocator_diagnostic.py --artifact <cold-artifact-root> \
    --output <new-output-directory>

Exactly one process per control/candidate_a and completion/inlay small. Sources,
ELFs and existing benchmark arguments are unchanged. GDB stores events in host
memory; traced timings/instructions are NOT acceptance samples. --inspect-artifact
validates frozen inputs and boundary extraction without native execution.
"""

import argparse
import json
import os
from pathlib import Path
import platform
import re
import shutil
import struct
import subprocess
import tarfile

from performance_calibration import run_logged, sha256, source_manifest

CONTROL = "bcaefa8882f70a963af596b3ea429bd90c731a79"
CANDIDATE = "b642de8a8f473199a07000faf5578e68246d7b40"
VARIANTS = ("control", "candidate_a")
WORKLOADS = (("completion_warm", 4), ("inlay_hints_warm", 8))


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2, allow_nan=False) + "\n")


def elf_layout(path):
    with path.open("rb") as handle:
        header = handle.read(64)
        if header[:7] != b"\x7fELF\x02\x01\x01":
            raise ValueError("Requires little-endian ELF64")
        kind, machine = struct.unpack_from("<HH", header, 16)
        if machine != 62 or kind not in (2, 3):
            raise ValueError("Requires native x86_64 EXEC/PIE ELF")
        phoff = struct.unpack_from("<Q", header, 32)[0]
        phsize, phcount = struct.unpack_from("<HH", header, 54)
        if phsize != 56 or not 0 < phcount < 256:
            raise ValueError("Unsupported ELF program headers")
        handle.seek(phoff)
        loads = []
        for _ in range(phcount):
            fields = struct.unpack("<IIQQQQQQ", handle.read(phsize))
            if fields[0] == 1:
                loads.append({"offset": fields[2], "vaddr": fields[3]})
    zero = [segment for segment in loads if segment["offset"] == 0]
    if len(zero) != 1 or zero[0]["vaddr"] % 4096:
        raise ValueError("Requires one page-aligned offset-zero PT_LOAD")
    return {"kind": "PIE" if kind == 3 else "EXEC", "offset_zero_vaddr": zero[0]["vaddr"]}


def query_boundary(nm_text, assembly, workload):
    name = f"analysis::{workload}::__iai_callgrind_wrapper_mod::{workload}"
    matches = []
    for line in nm_text.splitlines():
        row = re.fullmatch(r"([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+[tT]\s+(.+)", line)
        if row and row[3] == name:
            matches.append((int(row[1], 16), int(row[2], 16)))
    if len(matches) != 1:
        raise ValueError(f"Expected one physical query wrapper: {name}: {matches}")
    start, size = matches[0]
    instructions = []
    for line in assembly.splitlines():
        row = re.match(r"^\s*([0-9a-fA-F]+):\s+(.+)$", line)
        if row and start <= int(row[1], 16) < start + size:
            instructions.append((int(row[1], 16), row[2]))
    returns = [pc for pc, instruction in instructions if re.match(r"ret[q]?\b", instruction)]
    if not instructions or instructions[0][0] != start or not returns:
        raise ValueError(f"Missing entry/own return instructions: {name}")
    return {"symbol": name, "address": start, "size": size, "return_addresses": returns,
            "scope": "one wrapper entry through its own ret instruction; excludes setup/outer result drop"}


def prepare(artifact, output):
    metadata = json.loads((artifact / "dataset.json").read_text())
    if metadata.get("control") != CONTROL or metadata.get("candidate") != CANDIDATE:
        raise ValueError("Artifact revisions do not match frozen control/candidate")
    inputs = {}
    for role, revision in (("control", CONTROL), ("candidate", CANDIDATE)):
        archive = artifact / f"{role}.tar"
        archive_sha = sha256(archive)
        if archive_sha != metadata["archives"][role]["sha256"]:
            raise ValueError(f"Changed source archive: {role}")
        expected = json.loads((artifact / f"{role}-source-manifest.json").read_text())
        checkout = output / "sources" / role
        checkout.mkdir(parents=True)
        with tarfile.open(archive) as bundle:
            bundle.extractall(checkout, filter="data")
        actual = source_manifest(checkout)
        if actual != expected:
            raise ValueError(f"Extracted frozen source differs: {role}")
        write_json(output / f"{role}-source-manifest.json", actual)
        inputs[f"source_{role}"] = {"revision": revision, "archive": archive, "archive_sha256": archive_sha,
                                  "checkout": checkout, "source_manifest": actual}
    for variant in VARIANTS:
        build = metadata["builds"][variant]
        role = build["role"]
        frozen = artifact / build["retained_elf"]
        expected_sha = build["binary_sha256"]
        if sha256(frozen) != expected_sha or build["source"] != inputs[f"source_{role}"]["source_manifest"]["sha256"]:
            raise ValueError(f"Changed source/ELF provenance: {variant}")
        directory = output / "builds" / variant
        directory.mkdir(parents=True)
        executable = directory / "analysis.elf"
        shutil.copy2(frozen, executable)
        executable.chmod(executable.stat().st_mode | 0o100)
        if sha256(executable) != expected_sha:
            raise ValueError("Executable copy is not byte-identical")
        nm = (artifact / "builds" / variant / "nm.stdout").read_text()
        assembly = (artifact / "builds" / variant / "disassembly.stdout").read_text()
        inputs[variant] = {"role": role, "frozen": frozen, "executable": executable,
                           "sha256": expected_sha, "elf_layout": elf_layout(executable),
                           "boundaries": {name: query_boundary(nm, assembly, name) for name, _ in WORKLOADS},
                           "retained_nm_sha256": sha256(artifact / "builds" / variant / "nm.stdout"),
                           "retained_disassembly_sha256": sha256(artifact / "builds" / variant / "disassembly.stdout")}
    return inputs


def guard(inputs, variant):
    build = inputs[variant]
    role = inputs[f"source_{build['role']}"]
    current_source = source_manifest(role["checkout"])
    evidence = {"source_sha256": current_source["sha256"],
                "source_archive_sha256": sha256(role["archive"]),
                "input_elf_sha256": sha256(build["frozen"]),
                "executed_elf_sha256": sha256(build["executable"])}
    if current_source != role["source_manifest"]:
        raise ValueError("Frozen extracted source changed")
    if evidence["source_archive_sha256"] != role["archive_sha256"]:
        raise ValueError("Frozen source archive changed")
    if evidence["input_elf_sha256"] != build["sha256"] or evidence["executed_elf_sha256"] != build["sha256"]:
        raise ValueError("Frozen ELF changed")
    return evidence


def collect(artifact, output, inspect_only=False):
    if output.exists() or output.is_relative_to(artifact) or artifact.is_relative_to(output):
        raise ValueError("Output must be new and disjoint from retained artifact")
    output.mkdir(parents=True)
    dataset = {"diagnostic_only": True, "acceptance_comparator_run": False,
               "historical_hosted_elf": False, "control": CONTROL, "candidate": CANDIDATE,
               "mode": "static_inspection" if inspect_only else "native_gdb_request_trace",
               "fixed_invocations": [[variant, name, "small"] for variant in VARIANTS for name, _ in WORKLOADS],
               "samples": [], "errors": []}
    environment = dict(os.environ)

    def checkpoint():
        write_json(output / "dataset.json", dataset)

    checkpoint()
    try:
        inputs = prepare(artifact, output)
        dataset["inputs"] = {variant: {key: str(value) if isinstance(value, Path) else value
                                       for key, value in inputs[variant].items()} for variant in VARIANTS}
        if inspect_only:
            dataset["integrity"] = {variant: guard(inputs, variant) for variant in VARIANTS}
            checkpoint()
            return dataset
        if platform.system() != "Linux" or platform.machine() != "x86_64":
            raise ValueError("Native execution requires Linux x86_64; no ARM/emulation substitution")
        gdb_asset = Path(__file__).with_name("warm_allocator_gdb.py").resolve()
        dataset["gdb_asset_sha256"] = sha256(gdb_asset)
        dataset["environment"] = {}
        for name, command in (("gdb", ["gdb", "--version"]), ("kernel", ["uname", "-a"]),
                              ("glibc", ["ldd", "--version"]), ("os", ["cat", "/etc/os-release"] )):
            dataset["environment"][name] = run_logged(command, output, output / "environment" / f"{name}.stdout",
                                                       output / "environment" / f"{name}.stderr", environment)
        for variant in VARIANTS:
            build = inputs[variant]
            # Regenerate exact ELF symbol/disassembly evidence; do not trust an unrelated retained listing.
            nm = run_logged(["nm", "-C", "-S", "--defined-only", str(build["executable"])], output,
                            output / "builds" / variant / "nm.stdout", output / "builds" / variant / "nm.stderr", environment)
            assembly = run_logged(["objdump", "-d", "-C", "--no-show-raw-insn", str(build["executable"])], output,
                                  output / "builds" / variant / "disassembly.stdout",
                                  output / "builds" / variant / "disassembly.stderr", environment)
            for name, group_index in WORKLOADS:
                directory = output / "samples" / f"{variant}-{name}.small"
                directory.mkdir(parents=True)
                sample = {"variant": variant, "workload": f"{name}.small", "scope_valid": False}
                try:
                    before = guard(inputs, variant)
                    boundary = query_boundary(nm, assembly, name)
                    config = {"executable": str(build["executable"]), "elf_layout": build["elf_layout"],
                              "boundary": boundary, "output": str(directory / "trace.json"),
                              "max_events": 100000, "workload": sample["workload"]}
                    write_json(directory / "config.json", config)
                    env = dict(environment, WARM_ALLOCATOR_CONFIG=str(directory / "config.json"))
                    argv = ["gdb", "--batch", "-nx", "-nh", "-x", str(gdb_asset), "--args", str(build["executable"]),
                            "--iai-run", "analysis_hot_paths", str(group_index), "0", f"analysis::analysis_hot_paths::{name}"]
                    sample.update({"argv": argv, "boundary": boundary, "integrity_before": before})
                    checkpoint()
                    run_logged(argv, inputs[f"source_{build['role']}"]["checkout"], directory / "gdb.stdout", directory / "gdb.stderr", env)
                    trace = json.loads((directory / "trace.json").read_text())
                    if not trace.get("scope_valid") or trace.get("errors") or trace.get("inferior_exit_code") != 0:
                        raise ValueError("GDB trace boundary/completion failed; retain trace and logs")
                    sample.update({"scope_valid": True, "trace": str(directory / "trace.json"),
                                   "request_counts": trace["request_counts"], "integrity_after": guard(inputs, variant)})
                except (ValueError, OSError, subprocess.SubprocessError) as error:
                    sample["error"] = str(error)
                    dataset["errors"].append({"variant": variant, "workload": name, "error": str(error)})
                    try:
                        sample["integrity_after"] = guard(inputs, variant)
                    except (ValueError, OSError) as integrity_error:
                        sample["integrity_error"] = str(integrity_error)
                dataset["samples"].append(sample)
                checkpoint()
        if dataset["errors"] or len(dataset["samples"]) != 4:
            raise ValueError("Fixed diagnostic contains failures; no retries or acceptance filtering")
        return dataset
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        dataset["fatal_error"] = str(error)
        checkpoint()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--inspect-artifact", action="store_true")
    args = parser.parse_args()
    dataset = collect(args.artifact.resolve(), args.output.resolve(), args.inspect_artifact)
    print(json.dumps({"mode": dataset["mode"], "samples": dataset["samples"], "errors": dataset["errors"]}, indent=2))


if __name__ == "__main__":
    main()
