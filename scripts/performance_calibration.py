#!/usr/bin/env python3
"""Collect report-only, independently built Iai-Callgrind calibration pairs."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import stat
import subprocess
import sys
from typing import Any

from performance_policy import METRICS, _callgrind_metrics, _canonical_id

VARIANTS = ("a", "b", "layout", "extra_work", "extra_alloc")
PROBE = "calibration_layout_probe"
CACHE_ARGS = ["--cache-sim=yes"]
INLAY_ANCHOR = """fn inlay_hints_warm(fixture: WarmSnapshot) -> Vec<analysis::InlayHint> {
    std::hint::black_box(analysis::inlay_hints(
        std::hint::black_box(fixture.snapshot),
        analysis::TextRange::new(0, fixture.source.len()),
    ))
}"""
SETUP_ANCHORS = (
    """    fn small() -> Self {
        Self {
            source: SOURCE,
            snapshot: &SMALL_SNAPSHOT,
            completion_prefix: "transform_",
            identifier_name: "transform_31",
        }
    }""",
    """    fn medium() -> Self {
        Self {
            source: MEDIUM_SOURCE,
            snapshot: &MEDIUM_SNAPSHOT,
            completion_prefix: "worker_",
            identifier_name: "worker_0199",
        }
    }""",
    """    fn large() -> Self {
        Self {
            source: LARGE_SOURCE,
            snapshot: &LARGE_SNAPSHOT,
            completion_prefix: "worker_",
            identifier_name: "worker_0599",
        }
    }""",
)


def exact_replace(text: str, anchor: str, replacement: str) -> str:
    count = text.count(anchor)
    if count != 1:
        raise ValueError(f"calibration anchor must occur exactly once (found {count}): {anchor.splitlines()[0]}")
    return text.replace(anchor, replacement, 1)


def variant_harness(text: str, variant: str) -> str:
    if variant not in VARIANTS:
        raise ValueError(f"unknown calibration variant: {variant}")
    if PROBE in text:
        raise ValueError("source harness already contains a calibration layout probe")
    # Check all anchors even for A/A: controls must never silently target new code.
    exact_replace(text, INLAY_ANCHOR, INLAY_ANCHOR)
    for anchor in SETUP_ANCHORS:
        text = exact_replace(text, anchor, anchor.replace(
            "        Self {", "        std::hint::black_box(calibration_layout_probe as fn(u64) -> u64);\n        Self {", 1
        ))
    if variant == "layout":
        operations = []
        for index in range(256):
            constant = (0x9E3779B97F4A7C15 * (index + 1)) & ((1 << 64) - 1)
            operations.append(f"    state = state.rotate_left({index % 63 + 1}).wrapping_mul({constant | 1}_u64) ^ {constant}_u64;")
        body = "\n".join(operations) + "\n    state"
        argument = "mut state"
    else:
        body = "    state"
        argument = "state"
    probe = f"#[inline(never)]\nfn {PROBE}({argument}: u64) -> u64 {{\n{body}\n}}\n\n"
    text = exact_replace(text, "struct WarmSnapshot {", probe + "struct WarmSnapshot {")
    if variant == "extra_work":
        addition = """    std::hint::black_box(analysis::inlay_hints(
        std::hint::black_box(fixture.snapshot),
        analysis::TextRange::new(0, fixture.source.len()),
    ));
"""
    elif variant == "extra_alloc":
        addition = "    std::hint::black_box(vec![1_u64; 65_536]);\n"
    else:
        return text
    return exact_replace(text, INLAY_ANCHOR, INLAY_ANCHOR.replace("{\n", "{\n" + addition, 1))


def strict_json(text: str) -> Any:
    def unique_object(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    def invalid_constant(value):
        raise ValueError(f"invalid JSON constant: {value}")

    return json.loads(text, object_pairs_hook=unique_object, parse_constant=invalid_constant)


def parse_executable(text: str, checkout: Path, target: Path) -> Path:
    artifacts = []
    finished = False
    for line in text.splitlines():
        if not line.strip():
            continue
        message = strict_json(line)
        if not isinstance(message, dict):
            raise ValueError("Cargo emitted a non-object JSON message")
        if message.get("reason") == "build-finished":
            if message.get("success") is not True or finished:
                raise ValueError("Cargo build did not finish successfully exactly once")
            finished = True
        if message.get("reason") != "compiler-artifact":
            continue
        metadata = message.get("target", {})
        if metadata.get("name") == "analysis" and metadata.get("kind") == ["bench"]:
            executable = message.get("executable")
            if not isinstance(executable, str) or not executable:
                raise ValueError("analysis compiler-artifact has no executable")
            path = Path(executable)
            if not path.is_absolute():
                path = checkout / path
            artifacts.append(path.resolve())
    if not finished or len(artifacts) != 1:
        raise ValueError(f"expected one successful analysis compiler-artifact; found {len(artifacts)}")
    executable = artifacts[0]
    if not executable.is_relative_to(target.resolve()) or not executable.is_file() or not os.access(executable, os.X_OK):
        raise ValueError(f"Cargo executable is missing, not executable, or outside its independent target: {executable}")
    return executable


def parse_probe_symbols(text: str, large: bool) -> dict[str, Any]:
    symbols = []
    for line in text.splitlines():
        if PROBE not in line:
            continue
        match = re.fullmatch(r"\s*([0-9a-fA-F]+)\s+([0-9a-fA-F]+)\s+([tT])\s+(\S+)\s*", line)
        if not match or match[4].split("::")[-1] != PROBE:
            raise ValueError(f"unsupported nm layout-probe symbol record: {line}")
        size = int(match[2], 16)
        if int(match[1], 16) == 0:
            raise ValueError("layout probe has a zero address, not a retained executable symbol")
        if size <= 0 or (large and size < 1024):
            raise ValueError(f"layout probe has {size} bytes; requires {'at least 1024' if large else 'a retained nonzero symbol'}")
        symbols.append({"symbol": match[4], "address": match[1].lower(), "size": size})
    if len(symbols) != 1:
        raise ValueError(f"expected one retained layout-probe symbol in nm -C -S output; found {len(symbols)}")
    return symbols[0]


def parse_workload_list(text: str) -> set[tuple[str, str | None]]:
    identities = set()
    count = None
    for line in text.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"analysis::analysis_hot_paths::([A-Za-z0-9_]+)(?:::([A-Za-z0-9_]+))?: benchmark", line)
        if match:
            identity = (match[1], match[2])
            if identity in identities:
                raise ValueError(f"duplicate executed workload in --list: {identity}")
            identities.add(identity)
        elif re.fullmatch(r"0 tests, \d+ benchmarks", line) and count is None:
            count = int(line.split()[2])
        else:
            raise ValueError(f"unsupported benchmark --list output: {line}")
    if not identities or count != len(identities):
        raise ValueError("benchmark --list workload count is missing or inconsistent")
    if not {( "inlay_hints_warm", size) for size in ("small", "medium", "large")} <= identities:
        raise ValueError("executed harness is missing the three inlay_hints_warm positive-control workloads")
    return identities


def parse_measurement(text: str, baseline: str, executable: Path, expected: set[tuple[str, str | None]]) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    summaries = []
    workloads = []
    seen = set()
    for line in text.splitlines():
        if not line.strip():
            continue
        summary = strict_json(line)
        identity = _canonical_id(summary)
        if identity in seen or identity not in expected:
            raise ValueError(f"duplicate or unexpected executed workload: {identity}")
        seen.add(identity)
        if summary.get("baselines") != [baseline, baseline]:
            raise ValueError(f"{identity}: summary does not belong to requested fresh baseline {baseline}")
        if summary.get("kind") != "LibraryBenchmark" or summary.get("benchmark_exe") != str(executable):
            raise ValueError(f"{identity}: summary does not belong to the executed library benchmark {executable}")
        profiles = summary.get("profiles")
        if not isinstance(profiles, list) or len(profiles) != 1 or not isinstance(profiles[0], dict) or profiles[0].get("tool") != "Callgrind":
            raise ValueError(f"{identity}: requires exactly one Callgrind profile")
        profile_summary = profiles[0].get("summaries")
        parts = profile_summary.get("parts") if isinstance(profile_summary, dict) else None
        if not isinstance(parts, list) or len(parts) != 1:
            raise ValueError(f"{identity}: requires exactly one fresh Callgrind profile part")
        try:
            metrics = _callgrind_metrics(summary)
        except (KeyError, IndexError, TypeError) as error:
            raise ValueError(f"{identity}: malformed Callgrind summary") from error
        if not isinstance(metrics, dict):
            raise ValueError(f"{identity}: missing Callgrind metrics")
        counts = {}
        for metric in METRICS:
            data = metrics.get(metric)
            values = data.get("metrics") if isinstance(data, dict) else None
            if not isinstance(values, dict) or set(values) != {"Left"}:
                raise ValueError(f"{identity}: {metric} must be fresh Left-only metrics, not a stale comparison")
            value = values["Left"]
            if not isinstance(value, dict) or set(value) != {"Int"} or type(value["Int"]) is not int or value["Int"] < 0:
                raise ValueError(f"{identity}: {metric} count must be a nonnegative integer (not bool)")
            counts[metric] = value["Int"]
        workloads.append({"function_name": identity[0], "id": identity[1], "counts": counts})
        summaries.append(summary)
    if seen != expected:
        raise ValueError(f"executed measurement missing workloads: {sorted(expected - seen, key=str)}")
    return workloads, summaries


def check_probe_absent(text: str) -> None:
    """A retained function address must not turn into collected probe execution."""
    if not text.startswith("# callgrind format") or not re.search(r"^events:.*\bIr\b", text, re.MULTILINE):
        raise ValueError("missing or unsupported raw Callgrind profile")
    # Fail closed even for a zero-cost entry: absence is the control contract.
    # Search both definitions and compressed-name references (definitions retain names).
    if PROBE in text:
        raise ValueError("layout probe appears in collected Callgrind execution; the layout control is invalid")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def source_manifest(root: Path, excluded: tuple[Path, ...] = ()) -> dict[str, Any]:
    def fail_walk(error):
        raise error

    files = []
    for directory, names, filenames in os.walk(root, followlinks=False, onerror=fail_walk):
        base = Path(directory)
        names[:] = sorted(name for name in names if name != "__pycache__" and not (
            base == root and name in {".git", ".beads", "target"}
        ) and not any((base / name).resolve().is_relative_to(path) for path in excluded))
        for name in sorted(names + filenames):
            path = base / name
            if base == root and name in {".git", ".beads"}:
                continue
            if any(path.resolve().is_relative_to(exclusion) for exclusion in excluded):
                continue
            if path.is_symlink():
                raise ValueError(f"source symlinks are not supported in immutable calibration copies: {path}")
        for name in sorted(filenames):
            path = base / name
            if base == root and name in {".git", ".beads"}:
                continue
            if any(path.resolve().is_relative_to(exclusion) for exclusion in excluded):
                continue
            mode = path.stat().st_mode
            if not stat.S_ISREG(mode):
                raise ValueError(f"source contains a non-regular file: {path}")
            files.append({"path": path.relative_to(root).as_posix(), "sha256": sha256(path), "executable": bool(mode & 0o111)})
    files.sort(key=lambda entry: entry["path"])
    digest = hashlib.sha256(json.dumps(files, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return {"sha256": digest, "files": files}


def validate_directories(source: Path, work: Path, output: Path) -> None:
    if not source.is_dir():
        raise ValueError(f"source directory does not exist: {source}")
    for destination in (work, output):
        if source.is_relative_to(destination):
            raise ValueError(f"work/output directory must not contain the source: {destination}")
        if destination.exists() and (not destination.is_dir() or any(destination.iterdir())):
            raise ValueError(f"use a new or empty work/output directory to prevent stale artifact reuse: {destination}")
    if work.is_relative_to(output) or output.is_relative_to(work):
        raise ValueError("work and output directories must not contain each other")


def write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, allow_nan=False) + "\n", encoding="utf-8")


def run_logged(command: list[str], cwd: Path, stdout: Path, stderr: Path, environment: dict[str, str]) -> str:
    stdout.parent.mkdir(parents=True, exist_ok=True)
    write_json(stdout.with_suffix(".command.json"), {"argv": command, "cwd": str(cwd)})
    with stdout.open("wb") as out, stderr.open("wb") as err:
        result = subprocess.run(command, cwd=cwd, env=environment, stdout=out, stderr=err, check=False)
    if result.returncode != 0:
        raise ValueError(f"command exited {result.returncode}: {' '.join(command)}; see {stderr} and {stdout}")
    return stdout.read_text(encoding="utf-8")


def balanced_order(job_id: str, pair: int) -> list[str]:
    seed = int.from_bytes(hashlib.sha256(job_id.encode()).digest()[:8], "big")
    offset = (seed + pair - 1) % len(VARIANTS)
    order = list(VARIANTS[offset:] + VARIANTS[:offset])
    if (seed // len(VARIANTS) + (pair - 1) // len(VARIANTS)) % 2:
        order.reverse()
    return order


def collect(args: argparse.Namespace) -> None:
    source = args.source.resolve()
    work = args.work_dir.resolve()
    output = args.output_dir.resolve()
    validate_directories(source, work, output)
    work.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    dataset: dict[str, Any] = {"format_version": 1, "job_id": args.job_id, "role": args.role, "variants": {}, "samples": []}
    try:
        # Strip ambient Iai settings: filters, limits, baselines, and runners would
        # otherwise invalidate measurements even with explicit command options.
        environment = {key: value for key, value in os.environ.items() if not key.startswith("IAI_CALLGRIND_")}
        environment.pop("CARGO_TARGET_DIR", None)
        tools = {}
        for key, command in (("rustc", ["rustc", "--version", "--verbose"]), ("cargo", ["cargo", "--version"]), ("valgrind", ["valgrind", "--version"])):
            tools[key] = run_logged(command, source, output / "environment" / f"{key}.stdout", output / "environment" / f"{key}.stderr", environment).strip()
        installed = run_logged(["cargo", "install", "--list"], source, output / "environment" / "installed.stdout", output / "environment" / "installed.stderr", environment)
        runner_versions = re.findall(r"^iai-callgrind-runner v([^ :]+):$", installed, re.MULTILINE)
        if runner_versions != ["0.16.1"] or shutil.which("iai-callgrind-runner", path=environment.get("PATH")) is None:
            raise ValueError("requires cargo-installed iai-callgrind-runner 0.16.1 on PATH; inspect environment/installed.stdout")
        tools["iai_runner"] = f"iai-callgrind-runner {runner_versions[0]}"
        dataset["environment"] = {**tools, "os": platform.system(), "arch": platform.machine(), "cache_args": CACHE_ARGS}
        dataset["host"] = {"kernel": platform.release(), "hostname": platform.node(), "cpu": platform.processor(), "runner_identity": os.environ.get("RUNNER_NAME", "unknown")}
        if Path("/proc/cpuinfo").is_file():
            dataset["host"]["cpuinfo"] = Path("/proc/cpuinfo").read_text(encoding="utf-8")
        revision = subprocess.run(["git", "-C", str(source), "rev-parse", "HEAD"], capture_output=True, text=True, check=False)
        dataset["source_revision"] = revision.stdout.strip() if revision.returncode == 0 else "unknown"
        excluded = (work, output)
        original = source_manifest(source, excluded)
        write_json(output / "source-manifest.json", original)
        harness = (source / "benches" / "analysis.rs").read_text(encoding="utf-8")
        dataset["harness_sha256"] = sha256(source / "benches" / "analysis.rs")
        dataset["fixture_sha256"] = source_manifest(source / "benches" / "fixtures", excluded)["sha256"]
        # Validate source anchors before spending time on any build.
        generated = {variant: variant_harness(harness, variant) for variant in VARIANTS}
        manifests = {}
        checkouts = {}
        executables = {}
        expected = None
        for variant in VARIANTS:
            checkout = work / variant
            checkout.mkdir()
            for entry in original["files"]:
                relative = Path(entry["path"])
                destination = checkout / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source / relative, destination)
            if source_manifest(checkout) != original:
                raise ValueError(f"source changed while copying {variant}; discard this calibration job")
            (checkout / "benches" / "analysis.rs").write_text(generated[variant], encoding="utf-8")
            manifest = source_manifest(checkout)
            manifests[variant] = manifest
            write_json(output / "manifests" / f"{variant}.json", manifest)
            for entry in manifest["files"]:
                frozen = checkout / entry["path"]
                frozen.chmod(frozen.stat().st_mode & ~0o222)
            target = checkout / "target"
            build_environment = {**environment, "CARGO_TARGET_DIR": str(target)}
            build = run_logged(["cargo", "bench", "--locked", "--bench", "analysis", "--no-run", "--message-format=json", "--target-dir", str(target)], checkout, output / "builds" / f"{variant}.stdout", output / "builds" / f"{variant}.stderr", build_environment)
            executable = parse_executable(build, checkout, target)
            nm_output = run_logged(["nm", "-C", "-S", "--defined-only", str(executable)], checkout, output / "builds" / f"{variant}.nm.stdout", output / "builds" / f"{variant}.nm.stderr", environment)
            probe = parse_probe_symbols(nm_output, variant == "layout")
            dataset["variants"][variant] = {"source_sha256": manifest["sha256"], "binary_sha256": sha256(executable), "executable": str(executable), "layout_probe": probe}
            if source_manifest(checkout) != manifest:
                raise ValueError(f"{variant}: source manifest changed during build")
            listing = run_logged([str(executable), "--list"], checkout, output / "builds" / f"{variant}.list.stdout", output / "builds" / f"{variant}.list.stderr", environment)
            identities = parse_workload_list(listing)
            if expected is not None and identities != expected:
                raise ValueError(f"{variant}: independently built harness has a different executed workload manifest")
            expected = identities
            checkouts[variant] = checkout
            executables[variant] = executable
            write_json(output / "partial-data.json", dataset)
        if dataset["variants"]["a"]["source_sha256"] != dataset["variants"]["b"]["source_sha256"]:
            raise ValueError("A/A independent builds have different source manifests")
        for pair in range(1, args.pairs + 1):
            for order, variant in enumerate(balanced_order(args.job_id, pair)):
                checkout = checkouts[variant]
                executable = executables[variant]
                if source_manifest(checkout) != manifests[variant] or sha256(executable) != dataset["variants"][variant]["binary_sha256"]:
                    raise ValueError(f"{variant}: immutable source or compiled executable changed before measurement")
                name = f"pair{pair:03d}{variant.replace('_', '')}"
                raw = output / "raw" / name
                raw.mkdir(parents=True)
                home = raw / "iai"
                baseline = hashlib.sha256(f"{args.job_id}:{pair}:{variant}".encode()).hexdigest()
                stdout = raw / "stdout.jsonl"
                stderr = raw / "stderr.log"
                measured = run_logged([str(executable), "--output-format=json", "--save-summary=pretty-json", f"--save-baseline={baseline}", "--callgrind-args=--cache-sim=yes", f"--home={home}"], checkout, stdout, stderr, environment)
                workloads, summaries = parse_measurement(measured, baseline, executable, expected)
                # Only inspect paths emitted by this fresh process, never folder scans.
                profile_count = 0
                for summary in summaries:
                    paths = summary["profiles"][0].get("out_paths")
                    if not isinstance(paths, list) or not paths:
                        raise ValueError(f"{name}: executed summary has no raw Callgrind profiles")
                    for emitted in paths:
                        if not isinstance(emitted, str):
                            raise ValueError(f"{name}: malformed Callgrind output path")
                        path = Path(emitted).resolve()
                        if not path.is_relative_to(home) or not path.is_file():
                            raise ValueError(f"{name}: Callgrind output escaped fresh measurement home or is missing: {emitted}")
                        check_probe_absent(path.read_text(encoding="utf-8"))
                        profile_count += 1
                sample = {"pair": pair, "variant": variant, "order": order, "raw_stdout": stdout.relative_to(output).as_posix(), "raw_stderr": stderr.relative_to(output).as_posix(), "workloads": workloads, "verified_callgrind_profiles": profile_count}
                write_json(raw / "sample.json", sample)
                dataset["samples"].append(sample)
                write_json(output / "partial-data.json", dataset)
        for variant in VARIANTS:
            if source_manifest(checkouts[variant]) != manifests[variant] or sha256(executables[variant]) != dataset["variants"][variant]["binary_sha256"]:
                raise ValueError(f"{variant}: immutable source or executable changed during measurement")
            dataset["variants"][variant]["layout_probe_collected"] = False
        if source_manifest(source, excluded) != original:
            raise ValueError("original source changed during calibration; job cannot be compared")
        write_json(output / "data.json", dataset)
        print(f"Collected {args.pairs} complete pairs / {len(dataset['samples'])} fresh-process measurements in {output / 'data.json'}")
    except Exception as error:
        write_json(output / "partial-data.json", dataset)
        write_json(output / "failure.json", {"error": str(error), "type": type(error).__name__})
        raise


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--job-id", required=True)
    parser.add_argument("--role", choices=("discovery", "validation"), required=True)
    parser.add_argument("--pairs", type=int, default=5)
    args = parser.parse_args()
    if args.pairs < 1 or not args.job_id.strip():
        parser.error("--pairs must be positive and --job-id must be nonempty")
    try:
        collect(args)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"Calibration collection failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
