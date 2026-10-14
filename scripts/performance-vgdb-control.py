#!/usr/bin/env python3
"""Hosted, report-only original-ELF cache-entry investigation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import subprocess

HASHES = {
    "base": "263b5cdc4c0a4430e4e688ccfa331db44846147324235630174fcd38eeeb58e3",
    "candidate": "d85d41c996ac8b6341b4cd2bb4c0aca5b10873fd0ae12d0abf3056164f10d389",
}
CASES = {
    "completion_warm.large": "completion_warm",
    "inlay_hints_warm.large": "inlay_hints_warm",
    "scoped_completion_warm.small": "scoped_completion_warm",
    "semantic_tokens_warm.large": "semantic_tokens_warm",
    "import_completion_candidates_warm.fuzzy": "import_completion_candidates_warm",
}


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def metrics(path):
    lines = path.read_text().splitlines()
    events = next(line.split()[1:] for line in lines if line.startswith("events:"))
    counts = next(line.split()[1:] for line in lines if line.startswith("summary:"))
    return dict(zip(events, map(int, counts), strict=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise RuntimeError("Measurements require the declared hosted Linux runner")
    args.output.mkdir(parents=True, exist_ok=True)
    records = []
    for source, expected in HASHES.items():
        binary = (args.input / "binaries" / f"{source}-original" / "analysis").resolve()
        if digest(binary) != expected:
            raise RuntimeError(f"Unexpected {source} original ELF")
        binary.chmod(0o700)
        nm = subprocess.check_output(["nm", "--defined-only", str(binary)], text=True)
        (args.output / f"{source}-symbols.txt").write_text(nm)
        for case, function in CASES.items():
            profiles = list((args.input / "raw" / f"{source}-original" / "analysis" / "analysis_hot_paths" / case).glob("*.out.*"))
            if len(profiles) != 1:
                raise RuntimeError(f"Ambiguous retained profile: {case}")
            old = profiles[0]
            command = next(line[5:].strip() for line in old.read_text().splitlines() if line.startswith("cmd:"))
            child_args = shlex.split(command)[1:]
            symbols = [line.split()[-1] for line in nm.splitlines() if "__iai_callgrind_wrapper_mod" in line and line.split()[-1].endswith(f"{len(function)}{function}") and line.split()[-1].startswith("_RNvNtNt")]
            if len(symbols) != 1:
                raise RuntimeError(f"Ambiguous first-instruction guard: {case}: {symbols}")
            symbol = symbols[0]
            subprocess.run(["objdump", "-d", f"--disassemble={symbol}", str(binary)], stdout=(args.output / f"{source}-{case}-guard.txt").open("w"), check=True)
            for phase in ("normal", "keep-a", "cold", "keep-b"):
                folder = args.output / source / case / phase
                folder.mkdir(parents=True)
                profile = folder / "callgrind.out"
                command = ["setarch", "x86_64", "-R", "valgrind", "--tool=callgrind", "--error-exitcode=1", "--trace-children=yes", "--fair-sched=try", "--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64", "--cache-sim=yes", "--dump-line=yes", "--dump-instr=no", "--toggle-collect=*::__iai_callgrind_wrapper_mod::*", f"--callgrind-out-file={profile}"]
                if phase != "normal":
                    command += ["--vgdb=yes", "--vgdb-error=0"]
                command += [str(binary), *child_args]
                (folder / "command.json").write_text(json.dumps(command, indent=2))
                # Match Iai's default cleared child environment, without tokens.
                environment = {"PATH": os.environ["PATH"]}
                with (folder / "child.log").open("w") as log:
                    child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, env=environment)
                    try:
                        if phase != "normal":
                            commands = ["set pagination off", "set confirm off", "set language c", f"target remote | vgdb --wait=60 --pid={child.pid}", f"break *'{symbol}'", "continue", "printf \"BEND2_GUARD_PC=%p\\n\", $pc", "x/i $pc"]
                            if phase == "cold":
                                commands += ["monitor instrumentation off", "monitor instrumentation on"]
                            commands += ["delete breakpoints", "continue"]
                            gdb = folder / "control.gdb"
                            gdb.write_text("\n".join(commands) + "\n")
                            with (folder / "debugger.log").open("w") as debug_log:
                                subprocess.run(["gdb", "--quiet", "--batch", str(binary), "-x", str(gdb)], stdout=debug_log, stderr=subprocess.STDOUT, timeout=180, check=True, env=environment)
                            if "BEND2_GUARD_PC=" not in (folder / "debugger.log").read_text():
                                raise RuntimeError("Missing checked guard breakpoint")
                        if child.wait(timeout=180) != 0:
                            raise RuntimeError("Original benchmark child failed")
                    finally:
                        if child.poll() is None:
                            child.kill()
                            child.wait()
                if digest(binary) != expected:
                    raise RuntimeError("Original ELF mutated")
                records.append({"source": source, "case": case, "phase": phase, "binary_sha256": expected, "metrics": metrics(profile), "retained_metrics": metrics(old)})
                (args.output / "records.json").write_text(json.dumps(records, indent=2))
    checks = []
    for source in HASHES:
        for case in CASES:
            rows = {row["phase"]: row for row in records if row["source"] == source and row["case"] == case}
            checks.append({"source": source, "case": case, "same_guard_ir": len({row["metrics"]["Ir"] for row in rows.values()}) == 1, "debug_keep_cache_equal": rows["normal"]["metrics"] == rows["keep-a"]["metrics"] == rows["keep-b"]["metrics"]})
    (args.output / "report.json").write_text(json.dumps({"admission": False, "checks": checks, "records": records}, indent=2))
    if not all(check["same_guard_ir"] and check["debug_keep_cache_equal"] for check in checks):
        raise RuntimeError("External supervision or reset changed the guarded measurement; report is inconclusive")


if __name__ == "__main__":
    main()
