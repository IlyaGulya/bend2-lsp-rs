"""Disposable native semantic primitives; report-only, never an acceptance comparator.

Run on Ubuntu24.04 x86_64 with Rust1.98.1/Valgrind3.22.0:
  python3 scripts/semantic_lifecycle_diagnostic.py --source \"$PWD\" \\
      --source-ref b642de8a8f473199a07000faf5578e68246d7b40 --output /tmp/semantic-lifecycle-output
  (scratch defaults to OUTPUT-work; --work-dir overrides it)
Existing source/workflow/controller files are not changed. Only an exported tree is
augmented with private-access wrappers and an additional disposable bench target.
Collection toggles at the exact wrapper. Setup simulates caches: these are explicitly
setup_warmed_first_primitive samples, NOT claimed cold-cache-reset measurements.
"""

import argparse
from collections import Counter, defaultdict
import json
import os
from pathlib import Path
import platform
import re
import shutil

from performance_calibration import run_logged, sha256, source_manifest

BASELINE = "b642de8a8f473199a07000faf5578e68246d7b40"
EVENTS = "Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw".split()
FIXTURES = ("root", "aliases", "interleaved", "shadow", "base", "base_unaliased", "common")
STAGES = ("occurrences", "calls", "prepare", "import_lookup", "bind",
          "install_initial", "install_replace", "install_local", "remove")
BENCH = "semantic_lifecycle_stages"
REEXPORT = "\npub use semantic::lifecycle_diagnostic::semantic_lifecycle_diagnostic_main;\n"
BENCH_MAIN = "fn main() { bend2_lsp::workspace::semantic_lifecycle_diagnostic_main(); }\n"
BENCH_CONFIG = '\n[[bench]]\nname = "semantic_lifecycle_stages"\nharness = false\n'


def fixed_samples():
    for stage in STAGES:
        for fixture in FIXTURES:
            if stage == "install_local" and fixture != "common":
                continue
            if stage in {"import_lookup", "bind", "install_replace"} and fixture == "common":
                continue
            yield stage, fixture


def raw_profile(path):
    functions = defaultdict(lambda: [0] * len(EVENTS))
    lines = defaultdict(lambda: [0] * len(EVENTS))
    incoming = Counter()
    current = file = base_file = callee = ""
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
            summary += [0] * (len(EVENTS) - len(summary))
        elif row.startswith(("creator:", "cmd:", "desc:")):
            metadata.append(row)
        elif row.startswith("fl="):
            base_file = file = row[3:]
        elif row.startswith(("fi=", "fe=")):
            file = row[3:]
        elif row.startswith("fn="):
            current = row[3:]
            file = base_file
        elif row.startswith("cfn="):
            callee = row[4:]
        elif row.startswith("calls="):
            incoming[callee] += int(row[6:].split()[0])
            call_cost = True
        elif row and row[0].isdigit():
            if call_cost:
                call_cost = False
                continue
            if not positions or not current:
                raise ValueError(f"Missing function/position metadata: {row}")
            fields = row.split()
            values = [int(value) for value in fields[len(positions):]]
            values += [0] * (len(EVENTS) - len(values))
            if len(values) != len(EVENTS):
                raise ValueError(f"Invalid exclusive event count: {row}")
            line = int(fields[positions.index("line")]) if "line" in positions else 0
            for index, value in enumerate(values):
                functions[current][index] += value
                lines[current, file, line][index] += value
        elif row and row[0] in "+-*":
            raise ValueError("Compressed position encountered despite --compress-pos=no")
    if events != EVENTS or not positions or summary is None or len(summary) != len(EVENTS):
        raise ValueError("Missing supported positions/events/summary")
    exclusive = [sum(values[index] for values in functions.values()) for index in range(len(EVENTS))]
    if exclusive != summary:
        raise ValueError(f"Exclusive sums differ from summary: {exclusive} != {summary}")
    return {"events": dict(zip(EVENTS, summary)), "positions": positions,
            "metadata": metadata, "exclusive": dict(functions),
            "raw_incoming_calls": dict(incoming),
            "lines": [{"function": function, "file": file, "line": line,
                       "events": dict(zip(EVENTS, values))}
                      for (function, file, line), values in sorted(lines.items())]}


def executable_from_cargo(text, checkout, target):
    executables = []
    finished = False
    for line in text.splitlines():
        if not line.strip():
            continue
        message = json.loads(line)
        if message.get("reason") == "build-finished":
            if finished or message.get("success") is not True:
                raise ValueError("Cargo must finish successfully exactly once")
            finished = True
        if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == BENCH:
            if message["target"].get("kind") != ["bench"] or not message.get("executable"):
                raise ValueError("Invalid diagnostic bench artifact")
            executable = Path(message["executable"])
            if not executable.is_absolute():
                executable = checkout / executable
            executables.append(executable.resolve())
    if not finished or len(executables) != 1:
        raise ValueError("Expected exactly one successful diagnostic executable")
    executable = executables[0]
    if not executable.is_relative_to(target.resolve()) or not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError("Missing executable or outside independent target")
    return executable


def collect(source, work, output, revision):
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("Requires native Linux x86_64, not ARM/emulation")
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("Revision must be a full frozen commit SHA")
    if work.exists() or output.exists() or work.is_relative_to(output) or output.is_relative_to(work):
        raise ValueError("Work/output must be disjoint new directories")
    if source.is_relative_to(work) or source.is_relative_to(output) or work.is_relative_to(source) or output.is_relative_to(source):
        raise ValueError("Source/work/output must not contain one another")
    work.mkdir(parents=True)
    output.mkdir(parents=True)
    environment = dict(os.environ)
    asset = Path(__file__).with_name("semantic_lifecycle_stages.rs")
    dataset = {"diagnostic_only": True, "acceptance_comparator_run": False,
               "baseline_revision": BASELINE, "measured_revision": revision,
               "cache_state": "setup_warmed_first_primitive",
               "samples": [], "errors": [], "fixed_samples": list(fixed_samples())}

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
                      if key in {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_TARGET"} or key.startswith("CARGO_PROFILE_")},
        }
        if not dataset["environment"]["rustc"].startswith("rustc 1.98.1 ") or dataset["environment"]["valgrind"] != "valgrind-3.22.0":
            raise ValueError("Requires Rust1.98.1 and Valgrind3.22.0 pins")
        release = dict(line.split("=", 1) for line in dataset["environment"]["os_release"].splitlines() if "=" in line)
        if release.get("ID", "").strip('"') != "ubuntu" or release.get("VERSION_ID", "").strip('"') != "24.04":
            raise ValueError("Requires Ubuntu24.04")
        execute(["setarch", "x86_64", "-R", "true"], "environment/aslr")
        checkout = work / "source"
        checkout.mkdir()
        archive = output / "source.tar"
        execute(["git", "archive", "--format=tar", f"--output={archive}", revision], "archive/export")
        execute(["tar", "-xf", str(archive), "-C", str(checkout)], "archive/extract")
        exact = source_manifest(checkout)
        (output / "exact-source-manifest.json").write_text(json.dumps(exact, indent=2) + "\n")
        original = {name: (checkout / name).read_bytes() for name in
                    ("src/workspace/semantic.rs", "src/workspace.rs", "Cargo.toml")}
        additions = {"src/workspace/semantic.rs": b"\n" + asset.read_bytes(),
                     "src/workspace.rs": REEXPORT.encode(), "Cargo.toml": BENCH_CONFIG.encode()}
        bench_path = checkout / "benches" / f"{BENCH}.rs"
        if bench_path.exists() or "lifecycle_diagnostic" in original["src/workspace/semantic.rs"].decode():
            raise ValueError("Source already contains diagnostic augmentation")
        for name in original:
            (checkout / name).write_bytes(original[name] + additions[name])
        bench_path.write_text(BENCH_MAIN)
        frozen = source_manifest(checkout)
        exact_files = {entry["path"]: entry for entry in exact["files"]}
        frozen_files = {entry["path"]: entry for entry in frozen["files"]}
        changed = {name for name in exact_files.keys() | frozen_files.keys()
                   if exact_files.get(name) != frozen_files.get(name)}
        allowed = set(original) | {bench_path.relative_to(checkout).as_posix()}
        if changed != allowed:
            raise ValueError(f"Unexpected augmentation paths: {changed} != {allowed}")
        for name in original:
            if (checkout / name).read_bytes() != original[name] + additions[name]:
                raise ValueError(f"Production prefix was modified, not appended: {name}")
        (output / "augmented-source-manifest.json").write_text(json.dumps(frozen, indent=2) + "\n")
        dataset["source"] = {"archive_sha256": sha256(archive), "exact": exact["sha256"],
                             "augmented": frozen["sha256"], "asset_sha256": sha256(asset),
                             "allowed_append_paths": sorted(original), "new_bench": str(bench_path.relative_to(checkout))}
        target = work / "target"
        build = execute(["cargo", "bench", "--locked", "--bench", BENCH, "--no-run",
                         "--message-format=json", "--target-dir", str(target)], "build/cargo", checkout)
        executable = executable_from_cargo(build, checkout, target)
        if source_manifest(checkout) != frozen:
            raise ValueError("Augmented source changed during build")
        retained = output / "build" / "semantic-lifecycle.elf"
        shutil.copy2(executable, retained)
        frozen_elf = sha256(executable)
        if sha256(retained) != frozen_elf:
            raise ValueError("Retained ELF differs")
        dataset["build"] = {"executable": str(executable), "retained_elf": str(retained), "sha256": frozen_elf}
        symbols = execute(["nm", "-C", "-S", "--defined-only", str(executable)], "build/nm", checkout)
        execute(["objdump", "-d", "-C", "--no-show-raw-insn", str(executable)], "build/disassembly", checkout)
        execute(["readelf", "-SW", str(executable)], "build/sections", checkout)
        entries = {}
        for stage in STAGES:
            entry = f"bend2_lsp::workspace::semantic::lifecycle_diagnostic::stage_{stage}"
            matches = [line for line in symbols.splitlines() if line.endswith(" " + entry)]
            if len(matches) != 1:
                raise ValueError(f"Expected exact unique entry symbol {entry}; found {len(matches)}")
            entries[stage] = entry
        dataset["entries"] = entries
        checkpoint()
        for stage, fixture in fixed_samples():
            sample_name = f"{stage}-{fixture}"
            sample_dir = output / "profiles" / sample_name
            sample_dir.mkdir(parents=True)
            raw = sample_dir / "callgrind.out"
            log = sample_dir / "callgrind.log"
            command = ["setarch", "x86_64", "-R", "valgrind", "--tool=callgrind", "--cache-sim=yes",
                       "--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64",
                       "--collect-atstart=no", f"--toggle-collect={entries[stage]}", "--compress-pos=no",
                       "--compress-strings=no", "--dump-line=yes", "--dump-instr=yes", "--combine-dumps=no",
                       "--separate-threads=no", "--error-exitcode=200", f"--callgrind-out-file={raw}",
                       f"--log-file={log}", str(executable), stage, fixture]
            sample = {"stage": stage, "fixture": fixture, "argv": command, "scope_valid": False}
            try:
                if source_manifest(checkout) != frozen or sha256(executable) != frozen_elf or sha256(retained) != frozen_elf:
                    raise ValueError("Frozen source/ELF changed before sample")
                stdout = execute(command, f"profiles/{sample_name}/process", checkout)
                observation = json.loads(stdout)
                if observation.get("stage") != stage or observation.get("fixture") != fixture or observation.get("invariants_valid") is not True:
                    raise ValueError("Harness primitive invariants missing or failed")
                profile = raw_profile(raw)
                (sample_dir / "exclusive.json").write_text(json.dumps(profile, indent=2) + "\n")
                if "instr" not in profile["positions"] or profile["events"]["Ir"] <= 0 or profile["exclusive"].get(entries[stage], [0])[0] <= 0:
                    raise ValueError("Exact entry/instruction scope missing")
                outside = {name: values[0] for name, values in profile["exclusive"].items()
                           if values[0] and ("semantic_lifecycle_diagnostic_main" in name or name == "main")}
                if outside:
                    raise ValueError(f"Setup/observation leaked into collection: {outside}")
                if source_manifest(checkout) != frozen or sha256(executable) != frozen_elf:
                    raise ValueError("Frozen source/ELF changed during sample")
                sample.update({"scope_valid": True, "events": profile["events"], "observation": observation,
                               "raw": str(raw.relative_to(output)), "exclusive": str((sample_dir / "exclusive.json").relative_to(output))})
            except Exception as error:
                sample["error"] = str(error)
                dataset["errors"].append({"stage": stage, "fixture": fixture, "error": str(error)})
            dataset["samples"].append(sample)
            checkpoint()
        if dataset["errors"] or len(dataset["samples"]) != len(list(fixed_samples())):
            raise ValueError("Diagnostic contains errors; retained every sample, no retry-to-green")
        print(json.dumps({"dataset": str(output / "dataset.json"), "samples": len(dataset["samples"]),
                          "cache_state": dataset["cache_state"]}, indent=2))
    except Exception as error:
        dataset["fatal_error"] = str(error)
        checkpoint()
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, help="Optional disjoint scratch directory; default OUTPUT-work")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source-ref", default=BASELINE, help="Frozen full SHA; default is exact baseline b642de8")
    args = parser.parse_args()
    work = args.work_dir or args.output.with_name(args.output.name + "-work")
    collect(args.source.resolve(), work.resolve(), args.output.resolve(), args.source_ref)


if __name__ == "__main__":
    main()
