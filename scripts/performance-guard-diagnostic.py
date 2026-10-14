#!/usr/bin/env python3
"""Report-only guard reachability on an unmodified candidate benchmark ELF."""
import hashlib
import json
import os
from pathlib import Path
import subprocess

if os.environ.get("GITHUB_ACTIONS") != "true":
    raise RuntimeError("Hosted diagnostic only")
source = Path("candidate-source").resolve()
output = Path("target/cache-history-control").resolve()
output.mkdir(parents=True, exist_ok=True)
with (output / "build.jsonl").open("w") as log:
    subprocess.run(["cargo", "bench", "--locked", "--bench", "analysis", "--no-run", "--message-format=json"], cwd=source, stdout=log, check=True)
artifacts = [json.loads(line) for line in (output / "build.jsonl").read_text().splitlines() if line.startswith("{")]
executables = [row["executable"] for row in artifacts if row.get("reason") == "compiler-artifact" and row.get("target", {}).get("name") == "analysis" and row.get("executable")]
if len(executables) != 1:
    raise RuntimeError("Ambiguous original analysis artifact")
binary = Path(executables[0])
retained = output / "analysis"
retained.write_bytes(binary.read_bytes())
retained.chmod(0o700)
identity = hashlib.sha256(retained.read_bytes()).hexdigest()
for demangled, filename in [(False, "symbols.txt"), (True, "symbols-demangled.txt")]:
    command = ["nm", "--defined-only", "--print-size"] + (["-C"] if demangled else []) + [str(retained)]
    (output / filename).write_text(subprocess.check_output(command, text=True))
names = [line.split()[-1] for line in (output / "symbols.txt").read_text().splitlines() if line.split()[-1].startswith("_RNvNtNt") and line.split()[-1].endswith("27___iai_callgrind_wrapper_mod15completion_warm")]
if len(names) != 1:
    raise RuntimeError(f"Unexpected original completion guard: {names}")
for label, options in [
    ("default", ["--toggle-collect=*::__iai_callgrind_wrapper_mod::*"]),
    ("exact_raw", [f"--toggle-collect={names[0]}"]),
    ("exact_demangled", ["--toggle-collect=analysis::completion_warm::__iai_callgrind_wrapper_mod::completion_warm"]),
    ("no_inline", ["--read-inline-info=no", "--toggle-collect=*::__iai_callgrind_wrapper_mod::*"]),
    ("all_calls", ["--collect-atstart=yes"]),
    ("symbol_trace", ["--trace-symtab=yes", "--toggle-collect=*::__iai_callgrind_wrapper_mod::*"]),
]:
    profile = output / f"{label}.out"
    command = ["setarch", "x86_64", "-R", "valgrind", "--tool=callgrind", "--cache-sim=yes", "--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64", *options, f"--callgrind-out-file={profile}", str(retained), "--iai-run", "analysis_hot_paths", "4", "2", "analysis::analysis_hot_paths::completion_warm"]
    (output / f"{label}-command.json").write_text(json.dumps(command, indent=2))
    with (output / f"{label}.log").open("w") as log:
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, env={"PATH": os.environ["PATH"]}, check=True, timeout=180)
(output / "identity.json").write_text(json.dumps({"source_sha": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip(), "binary_sha256": identity}, indent=2))
if hashlib.sha256(retained.read_bytes()).hexdigest() != identity:
    raise RuntimeError("Original ELF mutated")
