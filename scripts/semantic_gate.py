#!/usr/bin/env python3
"""Bounded, offline semantic gate, separate from product performance policy.

RSS is a sampled simultaneous sum, not summed historical MaxRSS: Linux reads
/proc (including this supervisor); macOS uses a bounded persistent native
observer for current resident RSS (KiB-aligned). Descendants and owned process
groups remain tracked after reparenting. Polling sleeps at most
20ms between scans, plus process-table scan overhead; short peaks can be missed.
macOS scans share the phase deadline, not an independent per-scan cutoff; a slow
scan delays RSS detection, and the longest scan is reported. This is explicitly
NOT a cgroup hard allocation limit. Missing
monitoring fails closed. Output/artifact limits kill the run rather than trim a
counterexample. Linux subreaping permits waiting for orphaned grandchildren;
macOS waits direct children and checks the owned group has no live members.
"""
import argparse
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import resource
import selectors
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid

from semantic_observer import ObserverError, ProcessObserver

ROOT = Path(__file__).resolve().parents[1]
PROFILE_PATH = ROOT / "scripts/semantic_gate_profile.json"
PROFILE = json.loads(PROFILE_PATH.read_text())
if PROFILE.get("schema") != 1 or PROFILE.get("name") != "semantic-pr-v8-full-reference-budget" or PROFILE.get("workers") != 2:
    raise ValueError("unsupported semantic supervisor profile; review profile and runner together")
INTERRUPTED = None


def handle_shutdown_signal(signum, _frame):
    global INTERRUPTED
    INTERRUPTED = signal.Signals(signum).name


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, sort_keys=True, indent=2) + "\n")


def native_observer_command():
    executable = os.environ.get("BEND2_SEMANTIC_OBSERVER")
    if executable:
        path = Path(executable)
    else:
        target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT/"target"))
        path = target/"debug/bend2-perf"
    if not path.is_absolute():
        path = ROOT/path
    if not path.is_file():
        raise RuntimeError("native macOS RSS observer is missing; run "
            "'cargo build --locked --package bend2-perf --bin bend2-perf' "
            "or set BEND2_SEMANTIC_OBSERVER to that executable")
    return [str(path), "workflow", "observe-processes"]

class ProcessSnapshot(dict):
    """Observed workload processes plus the observer's simultaneous RSS."""
    observer_rss_bytes = 0


def is_live_process(row):
    # Lifecycle uses state and birth identity. RSS can race termination and must
    # never substitute for a live-process check, including zero-RSS live children.
    return row[4][:1] not in {"Z", "X", "x"}



def process_table(*, deadline=None, cancellable=True):
    """pid -> (ppid, process group, RSS bytes, birth identity, state)."""
    if sys.platform == "linux":
        result = ProcessSnapshot()
        page_size = os.sysconf("SC_PAGE_SIZE")
        for name in os.listdir("/proc"):
            if not name.isdigit():
                continue
            try:
                with open("/proc/"+name+"/stat", "rb", buffering=0) as stream:
                    text = stream.read().decode("utf-8")
                fields = text[text.rindex(")") + 2:].split(None, 22)
                # State, ppid, pgrp, ... starttime (22), rss (24).
                result[int(name)] = (int(fields[1]), int(fields[2]),
                    int(fields[21]) * page_size, fields[19], fields[0])
            except (FileNotFoundError, ProcessLookupError):
                continue
        if os.getpid() not in result:
            raise RuntimeError("/proc RSS monitoring unavailable")
        return result
    if sys.platform == "darwin":
        command = ["/bin/ps", "-axo", "pid=,ppid=,pgid=,rss=,state=,lstart="]
        # Standalone observations use the shutdown budget. Supervised scans
        # instead share the active phase's absolute deadline, including startup.
        deadline = time.monotonic()+PROFILE["shutdown_seconds"] if deadline is None else deadline
        def check_deadline():
            if cancellable and INTERRUPTED:
                raise RuntimeError(f"interrupted by {INTERRUPTED} during RSS monitoring")
            remaining = deadline-time.monotonic()
            if remaining <= 0:
                raise RuntimeError("hard deadline exceeded during RSS monitoring")
            return remaining
        check_deadline()
        # Retain the exact observer PID. A command-name filter would hide real
        # workload children and including this observer prevents quiescence.
        with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as observer:
            try:
                while True:
                    try:
                        stdout, stderr = observer.communicate(timeout=min(.02, check_deadline()))
                        break
                    except subprocess.TimeoutExpired:
                        # Continue waiting on this same scan, never retry ps or
                        # accept an absent/partial measurement. Check shutdown
                        # signals and the actual phase deadline every 20ms.
                        continue
                check_deadline()
            except BaseException:
                observer.kill()
                observer.communicate()
                raise
            if observer.returncode:
                raise subprocess.CalledProcessError(observer.returncode, command, stdout, stderr)
            observer_pid = observer.pid
        result = ProcessSnapshot()
        for row in stdout.splitlines():
            pid, parent, group, rss, state, birth = row.split(maxsplit=5)
            if int(pid) == observer_pid:
                result.observer_rss_bytes = int(rss)*1024
                continue
            result[int(pid)] = (int(parent), int(group), int(rss)*1024, birth, state)
        if os.getpid() not in result:
            raise RuntimeError("ps RSS monitoring unavailable")
        return result
    raise RuntimeError(f"aggregate RSS monitoring unsupported on {sys.platform}; use Linux or macOS")


def artifact_usage(directory, max_bytes, max_files):
    size = count = 0
    for path in directory.rglob("*"):
        if path.is_symlink():
            raise RuntimeError("artifact symlinks are forbidden")
        if path.is_file():
            count += 1
            size += path.stat().st_size
            if size > max_bytes or count > max_files:
                raise RuntimeError("artifact limit exceeded; original files retained, not truncated")
    return size, count


class ProcessTree:
    # Construction is deliberately observation-free: supervise binds the phase
    # deadline before the first scan, so startup sampling is charged and caught.
    def __init__(self):
        self.identities = {}
        self.groups = set()
        self.peak = 0
        self.children = set()
        self.samples = 0
        self.deadline = None
        self.cancellable = True
        self.max_sample_seconds = 0
        self.observer = None
        self.observation_errors = []
        self.shutdown_observer_attempted = False
        if sys.platform == "linux":
            libc = ctypes.CDLL(None, use_errno=True)
            if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER
                raise OSError(ctypes.get_errno(), "cannot enable child subreaping")

    def _table(self):
        if self.observer is None:
            return process_table(deadline=self.deadline, cancellable=self.cancellable)
        failed = self.observer.failed
        try:
            response = self.observer.observe(os.getpid(), self.identities, self.groups,
                deadline=self.deadline, cancellable=self.cancellable,
                interrupted=lambda: INTERRUPTED)
        except ObserverError as error:
            if not failed:
                self.observation_errors.append({
                    "stage": "work" if self.cancellable else "shutdown", "error": str(error)})
            raise
        table = ProcessSnapshot()
        for pid, parent, group, rss, birth, state in response["processes"]:
            if pid == response["observer_pid"]:
                table.observer_rss_bytes = rss
            else:
                table[pid] = (parent, group, rss, birth, state)
        return table

    def _prepare_shutdown_observer(self):
        if self.observer is None or not self.observer.failed or self.shutdown_observer_attempted:
            return False
        # A different stage verifies termination; failed work is never retried.
        self.shutdown_observer_attempted = True
        self.observer.close(deadline=self.deadline)
        self.observer = ProcessObserver(native_observer_command())
        return True

    def sample(self):
        started = time.monotonic()
        try:
            table = self._table()
        finally:
            self.max_sample_seconds = max(self.max_sample_seconds, time.monotonic()-started)
        owned = {os.getpid()}
        owned.update(pid for pid, birth in self.identities.items()
            if pid in table and table[pid][3] == birth)
        owned.update(pid for pid, row in table.items() if row[1] in self.groups)
        while True:
            added = {pid for pid, row in table.items() if row[0] in owned}
            if added <= owned:
                break
            owned.update(added)
        for pid in owned:
            if pid in table:
                self.identities[pid] = table[pid][3]
                if pid != os.getpid() and table[pid][1] != os.getpgrp():
                    self.groups.add(table[pid][1])
        self.children.update(owned - {os.getpid()})
        rss = sum(table[pid][2] for pid in owned if pid in table) + table.observer_rss_bytes
        if rss <= 0:
            raise RuntimeError("aggregate RSS monitoring returned no resident-memory evidence")
        self.peak = max(self.peak, rss)
        self.samples += 1
        return rss, table, owned

    def kill(self, *, quiescent=None):
        """Reuse a quiet observation only after worker exit and EOF are confirmed."""
        if quiescent is not None:
            table, owned = quiescent
        else:
            try:
                _, table, owned = self.sample()
            except (OSError, RuntimeError, subprocess.SubprocessError):
                table = None  # Monitoring failure must not prevent group cleanup.
                owned = ()
        quiet = table is not None and not any(
            pid != os.getpid() and pid in table and is_live_process(table[pid])
            for pid in owned)

        def finished_group(group, snapshot):
            members = [row for row in snapshot.values() if row[1] == group]
            return not members or all(not is_live_process(row) for row in members)

        errors = []
        for group in self.groups:
            # Skip only a positively observed zombie-only group. Absent groups
            # still get signaled, so unavailable monitoring cannot skip cleanup.
            if table is not None:
                members = [row for row in table.values() if row[1] == group]
                if members and all(not is_live_process(row) for row in members):
                    continue
            try:
                os.killpg(group, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except PermissionError as error:
                # Darwin can return EPERM when the last live member becomes a
                # zombie between sampling and killpg. Never ignore live EPERM.
                try:
                    if sys.platform != "darwin" or not finished_group(group, self._table()):
                        errors.append(str(error))
                except (OSError, RuntimeError, subprocess.SubprocessError) as monitoring_error:
                    errors.append(f"{error}; cannot verify group state: {monitoring_error}")
        if quiet and not errors:
            return True
        try:
            table = self._table()
        except (OSError, RuntimeError, subprocess.SubprocessError):
            table = {}
        for pid, birth in self.identities.items():
            if pid == os.getpid() or pid not in table or table[pid][3] != birth or not is_live_process(table[pid]):
                continue
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            except PermissionError as error:
                try:
                    current = self._table().get(pid)
                    if sys.platform != "darwin" or (current is not None and current[3] == birth and is_live_process(current)):
                        errors.append(str(error))
                except (OSError, RuntimeError, subprocess.SubprocessError) as monitoring_error:
                    errors.append(f"{error}; cannot verify child state: {monitoring_error}")
        if errors:
            raise RuntimeError("process-tree kill failed: "+"; ".join(errors))
        return False

    def reap(self):
        if sys.platform == "linux":
            while True:
                try:
                    pid, _ = os.waitpid(-1, os.WNOHANG)
                    if not pid:
                        break
                except ChildProcessError:
                    break


def supervise(command, *, deadline, output_path, artifact_dir, limits=None, tree=None, env=None):
    """Run one session; deadline is absolute monotonic, including shutdown."""
    limits = PROFILE if limits is None else limits
    started = time.monotonic()
    tree = ProcessTree() if tree is None else tree
    tree.deadline = deadline-limits["shutdown_seconds"]
    tree.cancellable = True
    tree.shutdown_observer_attempted = False
    cpu_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    status, reason, output_bytes = "error", None, 0
    process = None
    shutdown_started = None
    quiescent = None
    cleanup_errors = []
    output_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        if INTERRUPTED:
            raise RuntimeError(f"interrupted by {INTERRUPTED}")
        if deadline - started <= limits["shutdown_seconds"]:
            raise RuntimeError("deadline exhausted before spawn")
        if sys.platform == "darwin":
            tree.observer = ProcessObserver(native_observer_command())
        with output_path.open("wb") as output:
            process = subprocess.Popen(command, cwd=ROOT, stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True,
                env=env, close_fds=True)
            tree.groups.add(process.pid)
            os.set_blocking(process.stdout.fileno(), False)
            selector = selectors.DefaultSelector()
            selector.register(process.stdout, selectors.EVENT_READ)
            try:
                eof = False
                while True:
                    if INTERRUPTED:
                        raise RuntimeError(f"interrupted by {INTERRUPTED}")
                    if time.monotonic() >= deadline - limits["shutdown_seconds"]:
                        raise RuntimeError("hard deadline exceeded (shutdown reserve retained)")
                    rss, table, owned = tree.sample()
                    if time.monotonic() >= tree.deadline:
                        raise RuntimeError("hard deadline exceeded (shutdown reserve retained)")
                    if rss > limits["rss_bytes"]:
                        raise RuntimeError("aggregate sampled RSS limit exceeded")
                    artifact_usage(artifact_dir, limits["artifact_bytes"], limits["artifact_files"])
                    for key, _ in selector.select(min(limits["sample_seconds"],
                            max(0, tree.deadline-time.monotonic()))):
                        chunk = os.read(key.fd, 65536)
                        if not chunk:
                            eof = True
                            selector.unregister(key.fileobj)
                            continue
                        output_bytes += len(chunk)
                        if output_bytes > limits["output_bytes"]:
                            raise RuntimeError("subprocess output limit exceeded")
                        output.write(chunk)
                    if INTERRUPTED:
                        raise RuntimeError(f"interrupted by {INTERRUPTED}")
                    if time.monotonic() >= tree.deadline:
                        raise RuntimeError("hard deadline exceeded (shutdown reserve retained)")
                    if process.poll() is not None and eof:
                        if process.returncode:
                            raise RuntimeError(f"worker exited {process.returncode}")
                        # EOF includes the direct worker's final writes. Check
                        # them before cleanup can fail and replace this known
                        # resource violation with a monitoring error.
                        artifact_usage(artifact_dir, limits["artifact_bytes"], limits["artifact_files"])
                        # No live owned process can fork after this observation.
                        # EOF and successful leader exit now make it safe to
                        # retain that proof instead of repeating a global scan.
                        if not any(pid != os.getpid() and pid in table and
                                is_live_process(table[pid]) for pid in owned):
                            quiescent = (table, owned)
                        status = "pass"
                        break
            finally:
                selector.close()
                process.stdout.close()
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        reason = str(error)
    finally:
        shutdown_started = time.monotonic()
        tree.deadline = min(deadline, shutdown_started+limits["shutdown_seconds"])
        tree.cancellable = False
        verified_quiet = False
        try:
            tree._prepare_shutdown_observer()
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            status = "error"
            cleanup_errors.append(f"shutdown observer preparation failed: {error}")
            reason = reason or cleanup_errors[-1]
        try:
            verified_quiet = tree.kill(quiescent=quiescent)
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            status = "error"
            cleanup_errors.append(f"initial tree kill failed: {error}")
            reason = reason or cleanup_errors[-1]
        if tree.observer is not None and tree.observer.failed and not tree.shutdown_observer_attempted:
            # Preserve an initial cleanup failure even if its verifier succeeds.
            verified_quiet = False
            if status == "pass":
                status = "error"
                cleanup_errors.append("native observation failed during initial cleanup")
                reason = reason or cleanup_errors[-1]
            try:
                tree._prepare_shutdown_observer()
            except (OSError, RuntimeError, subprocess.SubprocessError) as error:
                status = "error"
                cleanup_errors.append(f"shutdown observer preparation failed: {error}")
                reason = reason or cleanup_errors[-1]
        # Always wait the direct worker, even if another group could not be
        # killed. A cleanup error must not itself leave an unreaped leader.
        try:
            if process is not None:
                process.wait(timeout=max(0.001, min(limits["shutdown_seconds"], deadline-time.monotonic())))
            while time.monotonic() < min(deadline, shutdown_started + limits["shutdown_seconds"]):
                tree.reap()
                if verified_quiet:
                    if time.monotonic() >= tree.deadline:
                        raise RuntimeError("process-tree shutdown deadline exceeded")
                    break
                _, table, owned = tree.sample()
                live = [pid for pid in owned if pid != os.getpid() and pid in table and is_live_process(table[pid])]
                if not live:
                    break
                tree.kill()
                time.sleep(0.005)
            else:
                status = "error"
                cleanup_errors.append("process-tree shutdown deadline exceeded")
                reason = reason or cleanup_errors[-1]
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            status = "error"
            cleanup_errors.append(f"shutdown failed: {error}")
            reason = reason or cleanup_errors[-1]
    if status == "pass":
        try:
            # EOF can race the worker's final write. Account only after every
            # owned process has stopped, without deleting/truncating evidence.
            artifact_usage(artifact_dir, limits["artifact_bytes"], limits["artifact_files"])
        except (OSError, RuntimeError) as error:
            status, reason = "error", str(error)
    if process is not None:
        # A child can become a zombie between the loop's reap and quiet scan.
        # Drain those adopted children too before CPU accounting; polling only
        # the leader leaves their resources waitable until a later phase.
        try:
            process.poll()
            tree.reap()
        except (OSError, subprocess.SubprocessError) as error:
            status = "error"
            cleanup_errors.append(f"final child reap failed: {error}")
            reason = reason or cleanup_errors[-1]
    observer = tree.observer
    tree.observer = None
    if observer is not None:
        try:
            observer.close(deadline=tree.deadline)
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            status = "error"
            cleanup_errors.append(f"native observer shutdown failed: {error}")
            reason = reason or cleanup_errors[-1]
    ended = time.monotonic()
    if ended > deadline:
        status = "error"
        cleanup_errors.append("hard deadline exceeded including shutdown")
        reason = reason or cleanup_errors[-1]
    cpu_after = resource.getrusage(resource.RUSAGE_CHILDREN)
    return {"status": status, "error": reason, "elapsed_seconds": ended-started,
        "cleanup_errors": cleanup_errors,
        "observer_errors": list(tree.observation_errors),
        "shutdown_seconds": ended-shutdown_started, "output_bytes": output_bytes,
        "peak_process_tree_rss_bytes": tree.peak, "rss_samples": tree.samples,
        "max_rss_sample_seconds": tree.max_sample_seconds,
        "child_count": len(tree.children), "child_cpu_seconds":
            cpu_after.ru_utime+cpu_after.ru_stime-cpu_before.ru_utime-cpu_before.ru_stime,
        "returncode": None if process is None else process.returncode}


def bounded_json(path, limit):
    if not path.is_file() or path.is_symlink():
        raise ValueError(f"missing regular report: {path}")
    if path.stat().st_size > limit:
        raise ValueError(f"report exceeds {limit} bytes: {path}")
    return json.loads(path.read_text())


def validate_report(plan, report):
    if plan.get("schema") != 1 or report.get("schema") != 1:
        raise ValueError("incompatible semantic plan/report schema")
    planned = plan["planned"]
    if not planned or report.get("status") != "pass":
        raise ValueError("no comparisons or semantic worker failed")
    expected = [(row["id"], row["input_sha256"]) for row in planned]
    actual = [(row["id"], row["input_sha256"]) for row in report["completed"]]
    declared = [(row["id"], row["input_sha256"]) for row in report["planned"]]
    if len({row[0] for row in expected}) != len(expected) or sorted(expected) != sorted(actual) or sorted(expected) != sorted(declared):
        raise ValueError("missing, duplicate, stale or unplanned case report")
    counts = report["counts"]
    lane_counts = {lane: sum(row.get("facet", row.get("lane", "valid")) == facet for row in planned)
        for lane, facet in (("valid", "valid"), ("invalid", "compiler-error"), ("recovery", "recovery"))}
    if counts.get("planned") != len(expected) or sum(lane_counts.values()) != len(expected):
        raise ValueError("incomplete planned count or unknown lane")
    for lane, count in lane_counts.items():
        if counts.get(f"{lane}_planned") != count:
            raise ValueError(f"incomplete {lane} planned count")
    if not lane_counts["valid"]:
        raise ValueError("zero valid comparisons")
    for name in ("validated", "compared", "matched"):
        if counts.get(name) != lane_counts["valid"]:
            raise ValueError(f"incomplete valid {name} count")
    if counts.get("intended_invalid") != lane_counts["invalid"] or counts.get("recovery") != lane_counts["recovery"]:
        raise ValueError("incomplete invalid or recovery lane")
    if report.get("failures"):
        raise ValueError("semantic failures present")
    for field in ("pin_sha256", "profile_sha256", "manifest_sha256"):
        if report.get(field) != plan[field]:
            raise ValueError(f"provenance mismatch: {field}")
    coverage = {}
    for row in planned:
        for family in row["case"]["families"]:
            entry = coverage.setdefault(family, dict.fromkeys(("planned", "validated", "compared", "matched"), 0))
            entry["planned"] += 1
            for name in ("validated", "compared", "matched"):
                entry[name] += row.get("facet", "valid") == "valid"
    if report.get("coverage") != coverage:
        raise ValueError("incomplete grammar-family coverage")
    stdio = report["stdio"]
    if not stdio["planned"] or not (stdio["planned"] == stdio["completed"] == stdio["passed"]):
        raise ValueError("incomplete real stdio scenarios")
    isolation = report["isolation"]
    if not all(isolation.get(check) for check in ("aba", "fresh", "repeated")):
        raise ValueError("missing worker isolation checks")
    valid_ids = [row["id"] for row in planned if row.get("facet", "valid") == "valid"]
    forward_hashes = {row["id"]: row["oracle_sha256"] for row in report["completed"]}
    expected_reverse = [{"id": key, "oracle_sha256": forward_hashes[key]} for key in reversed(valid_ids)]
    if isolation.get("reversed_views") != len(valid_ids) or isolation.get("reversed_history") != expected_reverse:
        raise ValueError("missing, reordered or divergent reverse oracle history")
    histories = report["metrics"]["histories"]
    forward, reverse = histories["forward"], histories["reverse"]
    if histories.get("workers") != PROFILE["workers"] or forward.get("planned") != len(planned) or forward.get("completed") != len(planned):
        raise ValueError("incomplete forward worker history")
    if reverse.get("planned") != len(valid_ids) or reverse.get("completed") != len(valid_ids) or reverse.get("complete") is not True:
        raise ValueError("incomplete reverse worker history")
    identities = [forward["pid"], reverse["pid"], histories["candidate_pid"], histories["fresh_pid"]]
    if any(type(pid) is not int or pid <= 0 for pid in identities) or len(set(identities)) != len(identities):
        raise ValueError("non-independent semantic workers")
    if [row["id"] for row in reverse["cases"]] != list(reversed(valid_ids)):
        raise ValueError("incomplete reverse history timing")

def default_candidate():
    return Path(os.environ.get("CARGO_TARGET_DIR", ROOT/"target"))/"release/semantic_probe"


def release_build_profile():
    actual = tomllib.loads((ROOT/"Cargo.toml").read_text())["profile"]["release"]
    if actual != PROFILE["candidate_build_profile"]["cargo_settings"]:
        raise ValueError("Cargo release settings differ from pinned semantic profile; review and version both together")
    return PROFILE["candidate_build_profile"]


def source_identity():
    digest = hashlib.sha256()
    paths = [ROOT/name for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml")]
    paths += sorted((ROOT/"src").rglob("*.rs")) + sorted((ROOT/"examples").rglob("*.rs"))
    # The candidate links the patched tower-lsp workspace crate, not crates.io's
    # source. Record every workspace member's manifests and Rust inputs so a
    # shared GITHUB_SHA cannot mask distinct local dependency bytes.
    workspace = tomllib.loads((ROOT/"Cargo.toml").read_text())["workspace"]
    for member in sorted(workspace["members"]):
        if member != ".":
            directory = ROOT/member
            paths.append(directory/"Cargo.toml")
            paths += sorted(directory.rglob("*.rs"))
    for path in paths:
        digest.update(str(path.relative_to(ROOT)).encode()+b"\\0"+path.read_bytes()+b"\\0")
    return digest.hexdigest()


def installer_provenance(path):
    report = bounded_json(path, PROFILE["report_bytes"])
    if report.get("schema") != 1 or report.get("status") != "pass" or not report.get("provenance"):
        raise ValueError("incomplete pinned installer provenance")
    return report["provenance"]



def environment():
    hardware = {}
    try:
        if sys.platform == "linux":
            memory = next(row for row in Path("/proc/meminfo").read_text().splitlines() if row.startswith("MemTotal:"))
            hardware["physical_memory_bytes"] = int(memory.split()[1])*1024
            hardware["cpu_model"] = next((row.split(":",1)[1].strip() for row in Path("/proc/cpuinfo").read_text().splitlines()
                if row.startswith("model name") or row.startswith("Hardware")), platform.processor())
        elif sys.platform == "darwin":
            for key, query in (("physical_memory_bytes", "hw.memsize"), ("cpu_model", "machdep.cpu.brand_string")):
                value = subprocess.run(["sysctl", "-n", query], capture_output=True, text=True, timeout=1, check=True).stdout.strip()
                hardware[key] = int(value) if key=="physical_memory_bytes" else value
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        hardware["metadata_error"] = str(error)
    return {"platform": platform.platform(), "machine": platform.machine(),
        "source_sha256": source_identity(),
        "python": platform.python_version(), "cpu_count": os.cpu_count(),
        "hardware": hardware, "candidate_build_profile": release_build_profile(),
        "rust_build_environment": {key:value for key,value in os.environ.items() if key in {
            "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_BUILD_RUSTFLAGS", "CARGO_BUILD_TARGET"} or key.startswith("CARGO_PROFILE_RELEASE_")},
        "rss_semantics": "sampled simultaneous process-tree RSS including supervisor; 20ms polling sleep plus scan overhead; short peaks may be missed",
        "ci": {**{key: os.environ.get(key) for key in ("GITHUB_SHA", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_JOB", "RUNNER_OS", "RUNNER_ARCH", "ImageOS", "ImageVersion")},
            "SEMANTIC_CALIBRATION_JOB": os.environ.get("SEMANTIC_CALIBRATION_JOB") or os.environ.get("CALIBRATION_JOB")}}


def run_gate(output, candidate, seed=PROFILE["seed"], projects=None, *, calibration_run_ordinal=None):
    started = time.monotonic()
    cpu_started = time.process_time()
    output.mkdir(parents=True, exist_ok=True)
    # Every attempt is fresh; stale reports can never satisfy a missing worker.
    # Never overwrite an earlier original failure/replay bundle.
    output = Path(tempfile.mkdtemp(prefix="attempt-", dir=output))
    deadline = started + PROFILE["hard_seconds"]
    result = {"schema": 1, "status": "error", "calibrated": False,
        "measurement_id": uuid.uuid4().hex, "calibration_run_ordinal": calibration_run_ordinal,
        "supervisor_profile_sha256": sha256(PROFILE_PATH.read_bytes()),
        "environment": environment(), "seed": seed, "stages": {}}
    result["artifact_directory"] = str(output)
    result["mode"] = "pr" if projects is None else "calibration-scaling"
    try:
        tree = ProcessTree()
        plan_path, worker_path = output / "plan.json", output / "worker-report.json"
        # Planning is deterministic/offline and charged to the same hard deadline.
        command = [sys.executable, str(ROOT / "scripts/semantic_conformance.py")]
        profile = "pr" if projects is None else "calibration"
        scaling_args = [] if projects is None else ["--projects", str(projects)]
        stage = supervise(command + ["plan", "--profile", profile, "--seed", str(seed),
            "--output", str(plan_path.resolve())] + scaling_args, deadline=deadline,
            output_path=output/"plan.log", artifact_dir=output, tree=tree)
        result["stages"]["plan"] = stage
        if stage["status"] != "pass":
            raise ValueError(stage["error"])
        plan = bounded_json(plan_path, PROFILE["report_bytes"])
        if plan["counts"]["projects"] != (PROFILE["generated_projects"] if projects is None else projects):
            raise ValueError("generated project count differs from pinned profile")
        manifest = {key:value for key,value in plan.items() if key != "manifest_sha256"}
        canonical = lambda value: json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()
        if sha256(canonical(manifest)) != plan["manifest_sha256"]:
            raise ValueError("plan manifest checksum mismatch")
        for row in plan["planned"]:
            if sha256(canonical(row["case"])) != row["input_sha256"]:
                raise ValueError("planned source checksum mismatch")
        stage = supervise(command + ["run", "--plan", str(plan_path.resolve()),
            "--candidate", str(candidate.resolve()), "--lsp", str(candidate.resolve().parent/"bend2-lsp"),
            "--report", str(worker_path.resolve()),
            "--artifact-dir", str((output/"failures").resolve()), "--deadline-seconds",
            str(max(0.001, deadline-time.monotonic()-PROFILE["shutdown_seconds"]))],
            deadline=deadline, output_path=output/"worker.log", artifact_dir=output, tree=tree)
        result["stages"]["semantic"] = stage
        if stage["status"] != "pass":
            raise ValueError(stage["error"])
        report = bounded_json(worker_path, PROFILE["report_bytes"])
        validate_report(plan, report)
        result.update(status="pass", counts=report["counts"], provenance=report["provenance"],
            workload_digest=plan["manifest_sha256"], peak_process_tree_rss_bytes=tree.peak,
            candidate_sha256=report["candidate_sha256"],
            worker_metrics=report["metrics"], source_bytes=sum(
                len(source.encode()) for row in plan["planned"] for source in row["case"]["files"].values()))
    except (OSError, ValueError, KeyError, TypeError, RuntimeError) as error:
        result["error"] = str(error)
    result["elapsed_seconds"] = time.monotonic()-started
    result["supervisor_cpu_seconds"] = time.process_time()-cpu_started
    if time.monotonic() > deadline:
        result.update(status="error", error="hard deadline exceeded during report validation")
    result["success_p95_target_seconds"] = PROFILE["success_p95_target_seconds"] if projects is None else None
    write_json(output/("gate-report.json" if projects is None else "scaling-report.json"), result)
    return result


def setup(output):
    started = time.monotonic()
    output.mkdir(parents=True, exist_ok=True)
    logs = Path(tempfile.mkdtemp(prefix="setup-logs-", dir=output))
    # Setup is explicitly outside the measured gate, but bounded and recorded.
    stages = {}
    install_report = output/"runtime-install-report.json"
    install_seconds = bounded_json(install_report, PROFILE["report_bytes"])["elapsed_seconds"] if install_report.exists() else 0
    limits = {**PROFILE, "rss_bytes": 8*PROFILE["rss_bytes"], "output_bytes": 16*PROFILE["output_bytes"],
        "artifact_bytes": 64*PROFILE["artifact_bytes"]}
    oracle_report = logs/"oracle-worker-report.json"
    for name, command in (("oracle", [sys.executable, "scripts/semantic_install.py", "--check", "--native-contracts", "--report", str(oracle_report.resolve())]),
            ("candidate", ["cargo", "build", "--release", "--locked", "--package", "bend2-lsp", "--package", "bend2-semantic-probe", "--bins"])):
        stages[name] = supervise(command, deadline=started+PROFILE["setup_target_seconds"]-install_seconds,
            output_path=logs/f"setup-{name}.log", artifact_dir=logs, limits=limits)
        if stages[name]["status"] != "pass":
            break
    report = {"schema": 1, "status": "pass" if len(stages)==2 and all(
        row["status"]=="pass" for row in stages.values()) else "error",
        "elapsed_seconds": time.monotonic()-started, "stages": stages,
        "total_tool_preparation_seconds": time.monotonic()-started+install_seconds,
        "environment": environment(), "log_directory": str(logs)}
    report["supervisor_profile_sha256"] = sha256(PROFILE_PATH.read_bytes())
    if report["status"] == "pass":
        try:
            report["provenance"] = installer_provenance(oracle_report)
            report["candidate_sha256"] = sha256(default_candidate().read_bytes())
        except (OSError, ValueError, KeyError) as error:
            report.update(status="error", error=str(error))
    report["elapsed_seconds"] = time.monotonic()-started
    report["total_tool_preparation_seconds"] = report["elapsed_seconds"]+install_seconds
    if report["total_tool_preparation_seconds"] > PROFILE["setup_target_seconds"]:
        report.update(status="error", error="tool preparation target exceeded including provenance checks")
    write_json(output/"setup-report.json", report)
    return report


def prepare_runtime(output):
    started = time.monotonic()
    output.mkdir(parents=True, exist_ok=True)
    logs = Path(tempfile.mkdtemp(prefix="runtime-logs-", dir=output))
    cache_present = any((ROOT/"target/semantic-tools/runtime").glob("*/bun"))
    oracle_report = logs/"installer-worker-report.json"
    result = supervise([sys.executable, "scripts/semantic_install.py", "--download-runtime", "--report", str(oracle_report.resolve())],
        deadline=started+PROFILE["setup_target_seconds"], output_path=logs/"runtime-install.log",
        artifact_dir=logs)
    result.update(schema=1, environment=environment(), target_seconds=PROFILE["setup_target_seconds"],
        runtime_cache_present_before_install=cache_present, log_directory=str(logs))
    result["supervisor_profile_sha256"] = sha256(PROFILE_PATH.read_bytes())
    if result["status"] == "pass":
        try:
            result["provenance"] = installer_provenance(oracle_report)
        except (OSError, ValueError, KeyError) as error:
            result.update(status="error", error=str(error))
    result["elapsed_seconds"] = time.monotonic()-started
    if result["elapsed_seconds"] > PROFILE["setup_target_seconds"]:
        result.update(status="error", error="runtime preparation target exceeded including provenance checks")
    write_json(output/"runtime-install-report.json", result)
    return result


def calibration_job(report):
    ci = report["environment"]["ci"]
    return (ci["GITHUB_RUN_ID"], ci["GITHUB_RUN_ATTEMPT"],
        ci.get("SEMANTIC_CALIBRATION_JOB") or ci["GITHUB_JOB"])


def distribution(values):
    values = sorted(values)
    if not values or any(not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0 for value in values):
        raise ValueError("missing or invalid calibration timing")
    return {"p50_seconds":values[math.ceil(.5*len(values))-1],
        "p95_seconds":values[math.ceil(.95*len(values))-1], "max_seconds":values[-1]}


def require_phase_identity(report, reference, *, candidate=True):
    if report.get("schema") != 1 or report.get("status") != "pass":
        raise ValueError("calibration contains a failed or incomplete supporting phase")
    if calibration_job(report) != calibration_job(reference):
        raise ValueError("supporting phase belongs to a different CI job")
    for field in ("GITHUB_SHA", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT"):
        if report["environment"]["ci"].get(field) != reference["environment"]["ci"][field]:
            raise ValueError("supporting phase CI provenance mismatch")
    source = reference["environment"].get("source_sha256")
    if not source or report["environment"].get("source_sha256") != source:
        raise ValueError("supporting phase candidate source mismatch")
    for field in ("provenance", "supervisor_profile_sha256"):
        if not reference.get(field) or report.get(field) != reference[field]:
            raise ValueError(f"supporting phase {field} mismatch")
    if candidate and (not reference.get("candidate_sha256") or report.get("candidate_sha256") != reference["candidate_sha256"]):
        raise ValueError("supporting phase candidate binary mismatch")


def calibration_support(job_directory, reference):
    """Require local downloaded artifacts, never original runner absolute paths."""
    reports, timings, preparation_totals = [], {}, []

    def phase(path, *, candidate=True):
        report = bounded_json(path, PROFILE["report_bytes"])
        require_phase_identity(report, reference, candidate=candidate)
        reports.append(str(path))
        return report

    installed = phase(job_directory/"runtime-install-report.json", candidate=False)
    timings["runtime_install"] = installed["elapsed_seconds"]
    for name, path in (("cold_setup", job_directory/"setup-report.json"),
            ("warm_setup", job_directory/"warm-setup/setup-report.json")):
        prepared = phase(path)
        if set(prepared["stages"]) != {"oracle", "candidate"} or any(
                stage.get("status") != "pass" for stage in prepared["stages"].values()):
            raise ValueError(f"incomplete {name} stage evidence")
        timings[name] = prepared["elapsed_seconds"]
        for stage, measurement in prepared["stages"].items():
            timings[f"{name}_{stage}"] = measurement["elapsed_seconds"]
        preparation_totals.append(prepared["total_tool_preparation_seconds"])
    summaries = sorted((job_directory/"scaling").rglob("scaling-summary.json"))
    if len(summaries) != 1:
        raise ValueError("missing or ambiguous startup/scaling evidence")
    summary = phase(summaries[0])
    if summary["startup"].get("status") != "pass" or not summary["startup"].get("peak_process_tree_rss_bytes"):
        raise ValueError("failed or unmonitored startup measurement")
    startup_path = summaries[0].parent/"startup-worker-report.json"
    startup = bounded_json(startup_path, PROFILE["report_bytes"])
    reports.append(str(startup_path))
    if startup.get("schema") != 1 or startup.get("status") != "pass" or startup.get("mode") != "startup-only" or (
            startup.get("generated_projects") != 0 or startup.get("compared_views") != 1 or startup.get("provenance") != reference["provenance"]):
        raise ValueError("incomplete startup-only compiler/probe evidence")
    timings["startup_supervised"] = summary["startup"]["elapsed_seconds"]
    for name, value in startup["metrics"].items():
        if name.endswith("_seconds"):
            timings[f"startup_{name}"] = value
    if [row.get("projects") for row in summary["scaling"]] != [128, 256, 512] or any(
            row.get("status") != "pass" for row in summary["scaling"]):
        raise ValueError("incomplete 128/256/512 scaling sequence")
    for projects in (128, 256, 512):
        paths = sorted((summaries[0].parent/f"projects-{projects}").rglob("scaling-report.json"))
        if len(paths) != 1:
            raise ValueError(f"missing or ambiguous {projects}-project raw scaling report")
        scaled = phase(paths[0])
        if scaled.get("mode") != "calibration-scaling" or scaled["counts"].get("generated") != projects:
            raise ValueError("scaling report workload mismatch")
        timings[f"scaling_{projects}"] = scaled["elapsed_seconds"]
    return timings, preparation_totals, reports


def calibration_report(paths):
    if len({path.resolve() for path in paths}) != len(paths):
        raise ValueError("duplicate raw calibration report")
    runs = [bounded_json(path, PROFILE["report_bytes"]) for path in paths]
    if len(runs) < 20:
        raise ValueError("calibration requires at least 20 raw runs")
    revisions = {row["environment"]["ci"]["GITHUB_SHA"] for row in runs}
    jobs = {calibration_job(row) for row in runs}
    if len(revisions)!=1 or None in revisions or len(jobs)<5 or any(None in job for job in jobs):
        raise ValueError("calibration requires one revision and at least 5 independent CI jobs")
    for field in ("workload_digest", "supervisor_profile_sha256", "candidate_sha256"):
        if len({row.get(field) for row in runs}) != 1 or not runs[0].get(field):
            raise ValueError(f"calibration changes or omits {field}")
    # Internal consistency alone would rescore historical evidence under the
    # current thresholds; the shared identity must be this exact profile.
    current_profile = sha256(PROFILE_PATH.read_bytes())
    if any(row["supervisor_profile_sha256"] != current_profile for row in runs):
        raise ValueError("calibration evidence was not produced under the current supervisor profile")
    if any(row.get("schema") != 1 or row["status"] != "pass" or row.get("mode") != "pr" for row in runs):
        raise ValueError("calibration includes failed/incomplete or non-PR runs")
    if not runs[0].get("provenance") or any(row.get("provenance") != runs[0]["provenance"] for row in runs):
        raise ValueError("calibration tool provenance mismatch")
    source = runs[0]["environment"].get("source_sha256")
    if not source or any(row["environment"].get("source_sha256") != source for row in runs):
        raise ValueError("calibration candidate source mismatch")
    measurement_ids, ordinals = set(), {job:set() for job in jobs}
    for row in runs:
        measurement_id = row.get("measurement_id")
        if not isinstance(measurement_id, str) or len(measurement_id) != 32:
            raise ValueError("missing or invalid calibration measurement identity")
        try:
            identity = uuid.UUID(hex=measurement_id)
        except ValueError as error:
            raise ValueError("invalid calibration measurement identity") from error
        if identity.hex != measurement_id or measurement_id in measurement_ids:
            raise ValueError("duplicate or invalid calibration measurement identity")
        measurement_ids.add(measurement_id)
        ordinal = row.get("calibration_run_ordinal")
        if type(ordinal) is not int or ordinal < 1:
            raise ValueError("missing or invalid calibration run ordinal")
        job = calibration_job(row)
        if ordinal in ordinals[job]:
            raise ValueError("duplicate calibration run ordinal within CI job")
        ordinals[job].add(ordinal)
    if any(len(values)<4 or values!=set(range(1,len(values)+1)) for values in ordinals.values()):
        raise ValueError("calibration requires at least four distinct sequential runs per CI job")
    roots, references = {}, {}
    for path, row in zip(paths, runs):
        root = next((parent for parent in path.parents if (parent/"runtime-install-report.json").is_file()), None)
        if root is None:
            raise ValueError("missing per-job installation/setup/startup/scaling evidence")
        job = calibration_job(row)
        if job in roots and roots[job] != root:
            raise ValueError("independent job reports use inconsistent evidence directories")
        roots[job], references[job] = root, row
    support, preparation_totals, supporting_reports = {}, [], []
    for job, root in roots.items():
        timings, prepared, files = calibration_support(root, references[job])
        for name, value in timings.items():
            support.setdefault(name, []).append(value)
        preparation_totals.extend(prepared)
        supporting_reports.extend(files)
    stage_distributions = {name:distribution(values) for name, values in support.items()}
    for stage in sorted(set.intersection(*(set(row["stages"]) for row in runs))):
        stage_distributions[f"gate_{stage}"] = distribution([row["stages"][stage]["elapsed_seconds"] for row in runs])
    totals = distribution([row["elapsed_seconds"] for row in runs])
    # Validate preparation timing before accepting the complete evidence set.
    distribution(preparation_totals)
    return {"schema": 1, "runs":len(runs), "jobs":len(jobs), "revision":next(iter(revisions)),
        **totals, "evidence_complete":True, "stage_distributions":stage_distributions,
        "targets_met":totals["p95_seconds"]<=PROFILE["success_p95_target_seconds"] and
            totals["max_seconds"]<=PROFILE["hard_seconds"] and all(
                0<row["peak_process_tree_rss_bytes"]<=PROFILE["rss_bytes"] for row in runs) and all(
                value<=PROFILE["setup_target_seconds"] for value in preparation_totals),
        "raw_reports":[str(path) for path in paths], "supporting_reports":supporting_reports, "policy_changed":False}


def scheduled(output, candidate):
    started = time.monotonic()
    deadline = started + PROFILE["scheduled_hard_seconds"]
    search_deadline = started + PROFILE["scheduled_search_seconds"]
    seed = PROFILE["seed"]
    results, shrink_result, error = [], None, None
    limits = {**PROFILE, "artifact_bytes": PROFILE["scheduled_artifact_bytes"],
        "artifact_files": PROFILE["scheduled_artifact_files"]}
    # The last complete PR batch must fit entirely inside the search allocation.
    try:
        while time.monotonic()+PROFILE["hard_seconds"] <= search_deadline:
            artifact_usage(output, limits["artifact_bytes"], limits["artifact_files"])
            run = run_gate(output/f"seed-{seed}", candidate, seed)
            results.append({"seed": seed, "status": run["status"], "artifact_directory": run["artifact_directory"]})
            if run["status"] != "pass":
                # Original is already durable; shrink cannot replace the failed status.
                bundles = sorted(Path(run["artifact_directory"]).rglob("original.json"))
                if bundles:
                    shrink_deadline = min(deadline, time.monotonic()+PROFILE["scheduled_shrink_seconds"])
                    shrink_result = supervise([sys.executable, "scripts/semantic_replay.py", "replay",
                        str(bundles[0].parent.resolve()), "--recorded-tools", "--shrink", "extended",
                        "--deadline-seconds", str(max(0, shrink_deadline-time.monotonic()-PROFILE["shutdown_seconds"]))],
                        deadline=shrink_deadline, output_path=output/"extended-shrink.log",
                        artifact_dir=output, limits=limits)
                break
            seed += 1
        artifact_usage(output, limits["artifact_bytes"], limits["artifact_files"])
    except (OSError, RuntimeError, ValueError) as failure:
        error = str(failure)
    report = {"schema": 1, "status": "pass" if results and not error and all(row["status"]=="pass" for row in results) else "error",
        "elapsed_seconds": time.monotonic()-started, "runs": results,
        "extended_shrink": shrink_result, "error": error,
        "search_limit_seconds": PROFILE["scheduled_search_seconds"],
        "shrink_limit_seconds": PROFILE["scheduled_shrink_seconds"],
        "hard_limit_seconds": PROFILE["scheduled_hard_seconds"]}
    if time.monotonic() > deadline:
        report.update(status="error", error="scheduled hard deadline exceeded")
    write_json(output/"scheduled-report.json", report)
    return report

def collect_scaling(output, candidate):
    output.mkdir(parents=True, exist_ok=True)
    output = Path(tempfile.mkdtemp(prefix="scaling-", dir=output))
    started = time.monotonic()
    report_path = output/"startup-worker-report.json"
    startup = supervise([sys.executable, "scripts/semantic_conformance.py", "startup",
        "--candidate", str(candidate.resolve()), "--report", str(report_path.resolve())],
        deadline=started+PROFILE["hard_seconds"], output_path=output/"startup.log",
        artifact_dir=output)
    results = {"schema":1, "status":"error", "mode":"calibration-only", "startup":startup, "scaling":[]}
    try:
        if startup["status"] != "pass":
            raise ValueError(startup["error"])
        observed = bounded_json(report_path, PROFILE["report_bytes"])
        if observed.get("status") != "pass" or observed.get("mode") != "startup-only" or observed.get("compared_views") != 1:
            raise ValueError("incomplete startup-only measurement")
        results["startup_worker_metrics"] = observed["metrics"]
        results.update(environment=environment(), provenance=observed["provenance"],
            candidate_sha256=sha256(candidate.read_bytes()),
            supervisor_profile_sha256=sha256(PROFILE_PATH.read_bytes()))
        for projects in (128, 256, 512):
            row = run_gate(output/f"projects-{projects}", candidate, projects=projects)
            results["scaling"].append({"projects":projects, "status":row["status"],
                "elapsed_seconds":row["elapsed_seconds"], "artifact_directory":row["artifact_directory"]})
            if row["status"] != "pass":
                raise ValueError(f"incomplete {projects}-project scaling measurement")
        results["status"] = "pass"
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        results["error"] = str(error)
    write_json(output/"scaling-summary.json", results)
    return results



def main():
    signal.signal(signal.SIGTERM, handle_shutdown_signal)
    signal.signal(signal.SIGINT, handle_shutdown_signal)
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    for name in ("run", "install", "setup", "scheduled", "collect", "scaling"):
        sub = commands.add_parser(name)
        sub.add_argument("--output", type=Path, default=ROOT/"target/semantic-gate")
        if name not in {"setup", "install"}:
            sub.add_argument("--candidate", type=Path, default=default_candidate())
        if name == "collect":
            sub.add_argument("--runs", type=int, default=4)
        if name == "run":
            sub.add_argument("--calibration-run-ordinal", type=int)
    sub = commands.add_parser("calibrate")
    sub.add_argument("reports", type=Path, nargs="+")
    sub.add_argument("--output", type=Path, required=True)
    sub = commands.add_parser("calibrate-directory")
    sub.add_argument("directory", type=Path)
    sub.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command in {"calibrate", "calibrate-directory"}:
        try:
            paths = args.reports if args.command == "calibrate" else sorted(args.directory.rglob("gate-report.json"))
            result = calibration_report(paths)
            write_json(args.output, result)
            return 0 if result["targets_met"] else 1
        except (ValueError, KeyError, TypeError, OSError) as error:
            write_json(args.output, {"status":"error", "error":str(error)})
            return 1
    if args.command == "collect":
        if args.runs < 1:
            parser.error("--runs must be positive")
        for index in range(args.runs):
            result = run_gate(args.output/f"run-{index:02d}", args.candidate,
                calibration_run_ordinal=index+1)
            if result["status"] != "pass":
                return 1
        return 0
    if args.command == "install":
        result = prepare_runtime(args.output)
    elif args.command == "setup":
        result = setup(args.output)
    elif args.command == "scheduled":
        result = scheduled(args.output, args.candidate)
    elif args.command == "scaling":
        result = collect_scaling(args.output, args.candidate)
    else:
        if args.calibration_run_ordinal is not None and args.calibration_run_ordinal < 1:
            parser.error("--calibration-run-ordinal must be positive")
        result = run_gate(args.output, args.candidate,
            calibration_run_ordinal=args.calibration_run_ordinal)
    print(json.dumps({"status":result["status"], "output":str(args.output)}))
    return 0 if result["status"] == "pass" else 1


if __name__ == "__main__":
    sys.exit(main())
