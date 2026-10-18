#!/usr/bin/env python3
"""Temporary original-harness diagnostic, never performance admission."""
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

assert os.environ.get("GITHUB_ACTIONS") == "true", "Hosted measurements only"
root = Path(os.environ["GITHUB_WORKSPACE"]).resolve()
output = root / "target/callgrind-diagnostic"
output.mkdir(parents=True, exist_ok=False)
work = Path(os.environ["RUNNER_TEMP"]) / "paired-work"
work.mkdir(exist_ok=False)
environment = {key: value for key, value in os.environ.items()
               if not key.startswith("IAI_CALLGRIND_") and key != "CARGO_TARGET_DIR"}
benchmarks = ("analysis", "reference_locations")
metrics = ("Ir", "Dr", "Dw", "I1mr", "D1mr", "D1mw", "ILmr", "DLmr", "DLmw")


def run(command, cwd, name, env=environment):
    destination = output / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.with_suffix(destination.suffix + ".command.json").write_text(
        json.dumps({"argv": list(map(str, command)), "cwd": str(cwd)}, indent=2))
    with destination.open("wb") as stdout, destination.with_suffix(
            destination.suffix + ".stderr").open("wb") as stderr:
        subprocess.run(command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                       stdout=stdout, stderr=stderr, check=True)
    return destination.read_text()


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def manifest(source):
    paths = subprocess.check_output(["git", "ls-files", "-z"], cwd=source).split(b"\0")
    return {os.fsdecode(path): digest(source / os.fsdecode(path))
            for path in sorted(paths) if path}


provenance = Path(os.environ["VALGRIND_INSTALL_ROOT"]) / "provenance.json"
(output / "valgrind-provenance.json").write_bytes(provenance.read_bytes())
run(["rustc", "--version", "--verbose"], root, "rustc-version.txt")
run(["cargo", "--version", "--verbose"], root, "cargo-version.txt")
run(["valgrind", "--version"], root, "valgrind-version.txt")
run(["cargo", "install", "--list"], root, "installed-tools.txt")

sources = {}
results = {}
for variant, key in (("a", "BASE_SHA"), ("b", "CANDIDATE_SHA")):
    sha = os.environ[key]
    assert re.fullmatch("[0-9a-f]{40}", sha), "Exact published source required"
    source = work / variant
    run(["git", "fetch", "--no-tags", "origin", sha], root, f"{variant}/fetch.log")
    run(["git", "worktree", "add", "--detach", str(source), sha], root,
        f"{variant}/checkout.log")
    assert run(["git", "rev-parse", "HEAD"], source, f"{variant}/head.log").strip() == sha
    initial = manifest(source)
    sources[variant] = {"sha": sha, "files": initial}
    (output / f"{variant}/source.json").write_text(json.dumps(sources[variant], indent=2))
    if variant == "b":
        assert {p: h for p, h in initial.items() if p.startswith("benches/")} == {
            p: h for p, h in sources["a"]["files"].items() if p.startswith("benches/")}
    target = source / "target"
    compiled = run(["cargo", "bench", "--locked", "--bench", "analysis", "--bench",
                    "reference_locations", "--no-run", "--message-format=json",
                    "--target-dir", str(target)], source, f"{variant}/build.jsonl",
                   {**environment, "CARGO_TARGET_DIR": str(target)})
    artifacts = [json.loads(line) for line in compiled.splitlines() if line.strip()]
    results[variant] = {}
    for benchmark in benchmarks:
        executables = {entry["executable"] for entry in artifacts
                       if entry.get("reason") == "compiler-artifact"
                       and entry["target"]["name"] == benchmark and entry.get("executable")}
        assert len(executables) == 1, "One original executable per benchmark"
        executable = Path(executables.pop()).resolve()
        assert executable.is_relative_to(target.resolve())
        binary_hash = digest(executable)
        run([str(executable), "--list"], source, f"{variant}/{benchmark}.list")
        baseline = hashlib.sha256(
            f'{os.environ["GITHUB_RUN_ID"]}/{os.environ["GITHUB_RUN_ATTEMPT"]}/{variant}/{benchmark}'.encode()
        ).hexdigest()
        home = output / variant / benchmark / "iai"
        measured = run([str(executable), "--output-format=json", "--save-summary=pretty-json",
                        f"--save-baseline={baseline}", "--callgrind-args=--cache-sim=yes",
                        f"--home={home}"], source, f"{variant}/{benchmark}/stdout.jsonl")
        for line in measured.splitlines():
            entry = json.loads(line)
            identity = f'{benchmark}/{entry["function_name"]}/{entry["id"]}'
            assert identity not in results[variant]
            profile, = entry["profiles"]
            counts = {key: profile["summaries"]["total"]["summary"]["Callgrind"][key]
                      ["metrics"]["Left"]["Int"] for key in metrics}
            path, = profile["out_paths"]
            path = Path(path).resolve()
            assert path.is_relative_to(home.resolve())
            lines = path.read_text().splitlines()
            events, = [line.split()[1:] for line in lines if line.startswith("events:")]
            totals = [line.split()[1:] for line in lines if line.startswith("totals:")]
            totals = totals or [line.split()[1:] for line in lines if line.startswith("summary:")]
            assert len(totals) == 1, "One preferred counter row per raw profile"
            values = list(map(int, totals[0]))
            assert 0 < len(values) <= len(events)
            # Callgrind omits trailing zeroes; match paired/report.rs normalization.
            values += [0] * (len(events) - len(values))
            raw = dict(zip(events, values, strict=True))
            assert counts == {key: raw[key] for key in metrics}, "Raw totals must agree"
            results[variant][identity] = counts
        assert digest(executable) == binary_hash, "Immutable measured executable"
        (output / f"{variant}/{benchmark}/binary.json").write_text(json.dumps(
            {"path": str(executable), "sha256": binary_hash}, indent=2))
        run(["nm", "-C", "--print-size", "--size-sort", str(executable)], source,
            f"{variant}/{benchmark}/symbols.txt")
    assert len(results[variant]) == 54, "Keep the complete original workload scope"
    assert manifest(source) == initial, "Immutable measured source"
assert results["a"].keys() == results["b"].keys()
report = {"kind": "diagnostic-only-not-admission", "pairs": 1, "sources": sources,
          "results": results}
(output / "diagnostic.json").write_text(json.dumps(report, indent=2))
rows = ["# Diagnostic only — not performance admission", "",
        "One original-harness pair. No gate verdict or control qualification.", "",
        "| Workload | Base Ir | Candidate Ir | Base I1mr | Candidate I1mr | Base ILmr | Candidate ILmr |",
        "|---|---:|---:|---:|---:|---:|---:|"]
for identity in sorted(results["a"]):
    a, b = results["a"][identity], results["b"][identity]
    rows.append(f'| {identity} | {a["Ir"]} | {b["Ir"]} | {a["I1mr"]} | {b["I1mr"]} | {a["ILmr"]} | {b["ILmr"]} |')
summary = "\n".join(rows) + "\n"
(output / "diagnostic.md").write_text(summary)
with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as handle:
    handle.write(summary)
print(summary)
