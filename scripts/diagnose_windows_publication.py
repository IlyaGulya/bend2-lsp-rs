#!/usr/bin/env python3
"""Diagnose who keeps a freshly executed Windows compiler executable open.

Diagnostic only. This script mirrors ``scripts/bootstrap_bend_source.py``
``build_windows()`` step for step against the same checksum-pinned source,
native Bun archive and genuine identity/version/base probes. It adds bounded
process and file-ownership observations around the real probes and around the
real publication rename, preserves the original publication error unchanged,
and never installs a compiler or writes ``BEND2_COMPILER_TEST_PATH``.

The observations are deliberately read-only:

* ``Win32_Process`` rows (name, pid, ppid, creation time, executable path; no
  command lines) from Windows CIM.
* Restart Manager (``RmGetList``) holder identities for the executable.
  Directories are excluded because that API rejects registered directories.
* An attributes-only ``CreateFileW`` probe (``dwShareMode == 0``). Its result
  does not prove the absence of image mappings or that renaming will succeed.
* A bounded release observation after a failed publication, plus a Defender
  configuration read. The observation is evidence, never a retry: publication
  is attempted exactly once and its original exception is re-raised.

Nothing here may turn a failed publication into a pass.
"""

import argparse
import base64
import ctypes
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import time
import traceback

import bootstrap_bend
import bootstrap_bend_source
from bootstrap_bend import (
    PINS,
    TARGETS,
    compiler_environment,
    download,
    extract_tar,
    native_target,
    probe_compiler_identity,
)
from bootstrap_bend_source import adjust_windows_locator, install_bun
from diagnose_windows_lint import pe_identity


# The production identity probe is one string constant of
# ``probe_compiler_identity``. Reusing that exact constant keeps this
# instrumented run byte-identical to the mandatory probe; source drift fails
# loudly instead of silently changing what was observed.
PROBE_SOURCE = next(
    constant for constant in probe_compiler_identity.__code__.co_consts
    if isinstance(constant, str) and "BEND2_COMPILER_IDENTITY_OUTPUT" in constant
)

PROCESS_NAMES = (
    "bend.exe",
    "bun.exe",
    "python.exe",
    "python3.exe",
    "MsMpEng.exe",
    "MpDefenderCoreService.exe",
    "MpCopyAccelerator.exe",
    "SecurityHealthService.exe",
)

PROCESS_SNAPSHOT_SCRIPT = r'''
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$names = @('bend.exe','bun.exe','python.exe','python3.exe','MsMpEng.exe','MpDefenderCoreService.exe','MpCopyAccelerator.exe','SecurityHealthService.exe')
$filter = ($names | ForEach-Object { "Name='" + $_ + "'" }) -join ' OR '
$rows = @(Get-CimInstance -ClassName Win32_Process -Filter $filter -Property ProcessId,ParentProcessId,CreationDate,Name,ExecutablePath | ForEach-Object {
    $path = $null
    if ($null -ne $_.ExecutablePath) { $path = [string]$_.ExecutablePath }
    [ordered]@{
        pid = [int]$_.ProcessId
        ppid = [int]$_.ParentProcessId
        created = [string]$_.CreationDate
        name = [string]$_.Name
        path = $path
    }
})
$truncated = $false
if ($rows.Count -gt 512) { $rows = $rows[0..511]; $truncated = $true }
[ordered]@{ processes = $rows; truncated = $truncated } | ConvertTo-Json -Depth 6 -Compress
'''

RESTART_MANAGER_SCRIPT = r'''
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class ReaRestartManager
{
    [StructLayout(LayoutKind.Sequential)]
    public struct RM_UNIQUE_PROCESS
    {
        public int dwProcessId;
        public System.Runtime.InteropServices.ComTypes.FILETIME ProcessStartTime;
    }

    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    public struct RM_PROCESS_INFO
    {
        public RM_UNIQUE_PROCESS Process;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 256)]
        public string strAppName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 64)]
        public string strServiceShortName;
        public int ApplicationType;
        public uint AppStatus;
        public uint TSSessionId;
        [MarshalAs(UnmanagedType.Bool)]
        public bool bRestartable;
    }

    [DllImport("rstrtmgr.dll", CharSet = CharSet.Unicode)]
    public static extern int RmStartSession(out uint pSessionHandle, int dwSessionFlags, string strSessionKey);

    [DllImport("rstrtmgr.dll")]
    public static extern int RmEndSession(uint pSessionHandle);

    [DllImport("rstrtmgr.dll", CharSet = CharSet.Unicode)]
    public static extern int RmRegisterResources(uint pSessionHandle, uint nFiles, string[] rgsFilenames, uint nApplications, RM_UNIQUE_PROCESS[] rgApplications, uint nServices, string[] rgsServiceNames);

    [DllImport("rstrtmgr.dll")]
    public static extern int RmGetList(uint dwSessionHandle, out uint pnProcInfoNeeded, ref uint pnProcInfo, [In, Out] RM_PROCESS_INFO[] rgAffectedApps, out uint lpdwRebootReasons);
}
'@
$files = @($env:REA_RESTART_MANAGER_FILES -split ';' | Where-Object { $_ })
if ($files.Count -eq 0) { throw 'no restart-manager files were supplied' }
$session = [uint32]0
$key = [guid]::NewGuid().ToString('N')
$rc = [ReaRestartManager]::RmStartSession([ref]$session, 0, $key)
if ($rc -ne 0) { throw "RmStartSession failed with $rc" }
try {
    $rc = [ReaRestartManager]::RmRegisterResources($session, [uint32]$files.Count, $files, [uint32]0, $null, [uint32]0, $null)
    if ($rc -ne 0) { throw "RmRegisterResources failed with $rc" }
    $needed = [uint32]0
    $count = [uint32]0
    $reasons = [uint32]0
    $rc = [ReaRestartManager]::RmGetList($session, [ref]$needed, [ref]$count, $null, [ref]$reasons)
    $infos = @()
    if ($rc -eq 234) {
        $count = $needed
        if ($count -gt 0) {
            $infos = New-Object 'ReaRestartManager+RM_PROCESS_INFO[]' ([int]$count)
            $rc = [ReaRestartManager]::RmGetList($session, [ref]$needed, [ref]$count, $infos, [ref]$reasons)
        } else {
            $rc = 0
        }
    }
    if ($rc -ne 0) { throw "RmGetList failed with $rc" }
    $rows = @()
    for ($index = 0; $index -lt $count; $index++) {
        $rows += [ordered]@{
            pid = [int]$infos[$index].Process.dwProcessId
            application = [string]$infos[$index].strAppName
            service = [string]$infos[$index].strServiceShortName
            application_type = [int]$infos[$index].ApplicationType
            status = [int]$infos[$index].AppStatus
            restartable = [bool]$infos[$index].bRestartable
        }
    }
    [ordered]@{ files = $files; reboot_reasons = [int]$reasons; processes = $rows } | ConvertTo-Json -Depth 6 -Compress
} finally {
    [void][ReaRestartManager]::RmEndSession($session)
}
'''

DEFENDER_SCRIPT = r'''
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$status = $null
$preference = $null
$errors = @()
try {
    $status = Get-MpComputerStatus | Select-Object AMServiceEnabled,AntivirusEnabled,RealTimeProtectionEnabled,IsTamperProtected,BehaviorMonitorEnabled,IoavProtectionEnabled,OnAccessProtectionEnabled,AntivirusSignatureVersion,AntivirusSignatureLastUpdated
} catch {
    $errors += "status: " + $_.Exception.Message
}
try {
    $preference = Get-MpPreference | Select-Object DisableRealtimeMonitoring,DisableIOAVProtection,DisableScriptScanning,ExclusionPath,ExclusionExtension,ExclusionProcess
} catch {
    $errors += "preference: " + $_.Exception.Message
}
[ordered]@{ status = $status; preference = $preference; errors = $errors } | ConvertTo-Json -Depth 6 -Compress
'''

_KERNEL32 = None


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat()


def bounded_text(text: str, limit: int = 262144) -> dict:
    encoded = text.encode("utf-8")
    return {
        "text": text[:limit],
        "truncated": len(encoded) > limit,
        "length": len(encoded),
        "sha256": hashlib.sha256(encoded).hexdigest(),
    }


def describe_exception(error: BaseException) -> dict:
    filename = getattr(error, "filename", None)
    filename2 = getattr(error, "filename2", None)
    return {
        "type": type(error).__name__,
        "winerror": getattr(error, "winerror", None),
        "errno": getattr(error, "errno", None),
        "strerror": getattr(error, "strerror", None),
        "filename": None if filename is None else str(filename),
        "filename2": None if filename2 is None else str(filename2),
        "message": str(error),
        "traceback": traceback.format_exc(),
    }


def file_sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def kernel32():
    global _KERNEL32
    if _KERNEL32 is None:
        from ctypes import wintypes
        library = ctypes.WinDLL("kernel32", use_last_error=True)
        library.CreateFileW.argtypes = [
            wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p,
            wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p,
        ]
        library.CreateFileW.restype = wintypes.HANDLE
        library.CloseHandle.argtypes = [wintypes.HANDLE]
        library.CloseHandle.restype = wintypes.BOOL
        _KERNEL32 = library
    return _KERNEL32


def exclusive_probe(path: Path, *, directory: bool = False) -> dict:
    """Record an attributes-only open with no sharing.

    This distinguishes observed sharing/access errors from a successful open.
    It does not identify the holder, exclude a mapped image section, or prove
    that DELETE access or directory publication is available.
    """
    file_read_attributes = 0x80
    open_existing = 3
    backup_semantics = 0x02000000
    library = kernel32()
    handle = library.CreateFileW(
        str(path), file_read_attributes, 0, None, open_existing,
        backup_semantics if directory else 0, None,
    )
    if handle == ctypes.c_void_p(-1).value:
        error = ctypes.get_last_error()
        return {
            "path": str(path),
            "directory": directory,
            "exclusive_open": False,
            "winerror": error,
            "message": ctypes.FormatError(error).strip(),
        }
    library.CloseHandle(handle)
    return {
        "path": str(path),
        "directory": directory,
        "exclusive_open": True,
        "winerror": 0,
        "message": "opened with no sharing",
    }


def bounded_lock_observation(path: Path, seconds: float) -> dict:
    """Sample the exclusive probe after a failure; never used to pass."""
    started = time.monotonic()
    samples = []
    opened_after_seconds = None
    while True:
        elapsed = time.monotonic() - started
        probe = exclusive_probe(path)
        samples.append({
            "elapsed_seconds": round(elapsed, 3),
            "exclusive_open": probe["exclusive_open"],
            "winerror": probe["winerror"],
        })
        if probe["exclusive_open"]:
            opened_after_seconds = round(elapsed, 3)
            break
        if elapsed >= seconds:
            break
        time.sleep(min(0.25, max(0.0, seconds - elapsed)))
    return {
        "path": str(path),
        "window_seconds": seconds,
        "attributes_open_available": opened_after_seconds is not None,
        "first_successful_attributes_open_seconds": opened_after_seconds,
        "samples": samples,
        "note": "attributes-only observation, not proof of rename availability; publication is not retried",
    }


def process_state(pid: int) -> dict:
    import _winapi
    query_limited = 0x1000
    try:
        handle = _winapi.OpenProcess(query_limited, False, pid)
    except OSError as error:
        return {"pid": pid, "state": "absent" if error.winerror == 87 else "unavailable", "winerror": error.winerror}
    try:
        code = _winapi.GetExitCodeProcess(handle)
        return {
            "pid": pid,
            "state": "running" if code == _winapi.STILL_ACTIVE else "exited",
            "exit_code": code,
        }
    finally:
        _winapi.CloseHandle(handle)


def powershell_environment(environment: dict) -> dict:
    """Add the module path the Defender cmdlets need, never credentials."""
    result = dict(environment)
    module_path = os.environ.get("PSModulePath")
    if module_path:
        result["PSModulePath"] = module_path
    return result


def powershell(script: str, environment: dict, timeout: float):
    return subprocess.run(
        ["pwsh", "-NoProfile", "-NonInteractive", "-Command", script],
        env=powershell_environment(environment), capture_output=True, text=True,
        timeout=timeout,
    )


def snapshot(label: str, environment: dict, evidence_root: Path) -> dict:
    record = {
        "label": label,
        "timestamp_utc": utc_now(),
        "python_pid": os.getpid(),
        "python_ppid": os.getppid(),
    }
    try:
        completed = powershell(PROCESS_SNAPSHOT_SCRIPT, environment, timeout=30)
    except BaseException as error:
        record["error"] = describe_exception(error)
        return record
    record["returncode"] = completed.returncode
    if completed.returncode:
        record["stderr"] = bounded_text(completed.stderr)
        return record
    try:
        parsed = json.loads(completed.stdout)
    except json.JSONDecodeError:
        record["stdout"] = bounded_text(completed.stdout)
        return record
    processes = parsed.get("processes", [])
    names = {row["pid"]: row["name"] for row in processes}
    for row in processes:
        row["parent_name"] = names.get(row["ppid"])
    record["truncated"] = parsed.get("truncated", False)
    record["processes"] = processes
    record["bend_processes"] = [row for row in processes if row["name"].lower() == "bend.exe"]
    (evidence_root / "snapshots" / f"{label}.json").write_text(
        json.dumps(record, indent=2) + "\n", encoding="utf-8",
    )
    return record


def restart_manager_holders(paths: list[Path], environment: dict) -> dict:
    script_environment = {
        **environment,
        "REA_RESTART_MANAGER_FILES": ";".join(str(path) for path in paths),
    }
    try:
        completed = powershell(RESTART_MANAGER_SCRIPT, script_environment, timeout=60)
    except BaseException as error:
        return {"result": "error", "error": describe_exception(error)}
    record = {"returncode": completed.returncode}
    if completed.returncode:
        record["result"] = "error"
        record["stderr"] = bounded_text(completed.stderr)
        return record
    try:
        record["result"] = "ok"
        record["holders"] = json.loads(completed.stdout)
    except json.JSONDecodeError:
        record["result"] = "unparsed"
        record["stdout"] = bounded_text(completed.stdout)
    return record


def defender_status(environment: dict) -> dict:
    try:
        completed = powershell(DEFENDER_SCRIPT, environment, timeout=60)
    except BaseException as error:
        return {"result": "error", "error": describe_exception(error)}
    record = {"returncode": completed.returncode}
    if completed.returncode:
        record["result"] = "error"
        record["stderr"] = bounded_text(completed.stderr)
        return record
    try:
        record["result"] = "ok"
        record["status"] = json.loads(completed.stdout)
    except json.JSONDecodeError:
        record["result"] = "unparsed"
        record["stdout"] = bounded_text(completed.stdout)
    return record


def leftover_probe(parent: Path) -> list[dict]:
    """Describe staging directories a previous attempt left behind."""
    entries = []
    for candidate in sorted(parent.glob("bend-source-*")):
        if not candidate.is_dir() or candidate.is_symlink():
            continue
        record = {
            "path": str(candidate),
            "modified_utc": datetime.fromtimestamp(
                candidate.stat().st_mtime, timezone.utc,
            ).isoformat(),
        }
        output = candidate / "distribution" / "bin" / "bend.exe"
        record["distribution_output_present"] = output.is_file()
        if output.is_file():
            record["exclusive_output"] = exclusive_probe(output)
            record["output_size"] = output.stat().st_size
            try:
                record["output_sha256"] = file_sha256(output)
            except OSError as error:
                record["output_sha256_error"] = describe_exception(error)
        files = []
        for path in candidate.rglob("*"):
            if len(files) >= 200:
                record["listing_truncated"] = True
                break
            if path.is_file() and not path.is_symlink():
                files.append({
                    "relative": str(path.relative_to(candidate)),
                    "size": path.stat().st_size,
                })
        record["files"] = files
        entries.append(record)
        if len(entries) >= 8:
            break
    return entries


def run_recorded(
    arguments, *, label, cwd, environment, timeout, capture, check,
    evidence_root, report,
):
    """subprocess.run with the same wait/check semantics plus PID receipts."""
    started = time.monotonic()
    record = {
        "label": label,
        "arguments": [str(argument) for argument in arguments],
        "cwd": str(cwd),
        "capture": capture,
        "parent_pid": os.getpid(),
        "started_utc": utc_now(),
    }
    process = subprocess.Popen(
        [str(argument) for argument in arguments], cwd=cwd, env=environment,
        stdout=subprocess.PIPE if capture else None,
        stderr=subprocess.PIPE if capture else None,
        text=capture,
    )
    record["pid"] = process.pid
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        process.kill()
        stdout, stderr = process.communicate()
    record["elapsed_seconds"] = round(time.monotonic() - started, 3)
    record["returncode"] = process.returncode
    record["timed_out"] = timed_out
    record["process_state_after"] = process_state(process.pid)
    if capture:
        record["stdout"] = bounded_text(stdout or "")
        record["stderr"] = bounded_text(stderr or "")
        (evidence_root / "logs" / f"{label}.stdout").write_text(
            stdout or "", encoding="utf-8",
        )
        (evidence_root / "logs" / f"{label}.stderr").write_text(
            stderr or "", encoding="utf-8",
        )
    report["runs"].setdefault(label, []).append(record)
    if timed_out:
        raise subprocess.TimeoutExpired(record["arguments"], timeout)
    if check and process.returncode:
        raise subprocess.CalledProcessError(
            process.returncode, record["arguments"], output=stdout, stderr=stderr,
        )
    return record, stdout, stderr


def probe_identity_recorded(
    output: Path, staging: Path, environment: dict, evidence_root: Path, report: dict,
) -> dict:
    """Mirror bootstrap_bend.probe_compiler_identity with a PID receipt."""
    probe = staging / "identity.ts"
    result = staging / "embedded-identity.json"
    source_output = output.parent.parent / "compiler-embedded-source.js"
    probe.write_text(PROBE_SOURCE, encoding="utf-8")
    module = f"await import({json.dumps(probe.as_uri())});"
    preload = base64.b64encode(module.encode("utf-8")).decode("ascii")
    probe_environment = {
        **environment,
        "BUN_OPTIONS": f"--preload data:text/javascript;base64,{preload}",
        "BEND2_COMPILER_IDENTITY_OUTPUT": str(result),
        "BEND2_COMPILER_SOURCE_OUTPUT": str(source_output),
    }
    run_recorded(
        [str(output), "version"], label="identity_probe", cwd=staging,
        environment=probe_environment, timeout=30, capture=False, check=True,
        evidence_root=evidence_root, report=report,
    )
    observed = json.loads(result.read_text(encoding="utf-8"))
    with source_output.open("rb") as source:
        fingerprint = hashlib.file_digest(source, "sha256").hexdigest()
    if observed["fingerprint"] != fingerprint:
        raise ValueError("embedded compiler source identity mismatch")
    report["identity_probe"] = {
        "observed": observed,
        "embedded_source_sha256": fingerprint,
        "embedded_source_size": source_output.stat().st_size,
    }
    return observed


def copy_publication_experiment(
    distribution: Path, destination: Path, environment: dict, evidence_root: Path,
) -> dict:
    """Publish an identical, never-executed copy to separate scan from image."""
    copy_root = Path(tempfile.mkdtemp(prefix="bend-copy-", dir=destination.parent))
    copy = copy_root / "distribution"
    target = destination.with_name(destination.name + "-copy-probe")
    record = {"copy_root": str(copy_root), "copy": str(copy), "probe_target": str(target)}
    try:
        shutil.copytree(distribution, copy)
        record["copytree"] = "ok"
        record["exclusive_copy_output"] = exclusive_probe(copy / "bin" / "bend.exe")
        try:
            copy.rename(target)
        except BaseException as error:
            record["rename"] = "failed"
            record["rename_error"] = describe_exception(error)
            record["holders"] = restart_manager_holders(
                [copy / "bin" / "bend.exe"], environment,
            )
        else:
            record["rename"] = "succeeded"
            record["renamed_to"] = str(target)
    except BaseException as error:
        record["copytree"] = "failed"
        record["copytree_error"] = describe_exception(error)
    finally:
        cleanup = []
        for candidate in (copy_root, target):
            if not candidate.exists() and not candidate.is_symlink():
                continue
            try:
                shutil.rmtree(candidate)
                cleanup.append({"path": str(candidate), "result": "removed"})
            except BaseException as error:
                cleanup.append({
                    "path": str(candidate),
                    "result": "failed",
                    "error": describe_exception(error),
                })
        record["cleanup"] = cleanup
    return record


def copy_if_present(source: Path, destination: Path) -> None:
    if source.is_file() and not source.is_symlink():
        shutil.copyfile(source, destination)


def write_report(report: dict, evidence_root: Path) -> None:
    (evidence_root / "publication-evidence.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8",
    )
    publication = report.get("publication", {})
    error = publication.get("error")
    summary = {
        "publication": publication.get("result"),
        "publication_error": None if not error else {
            "type": error.get("type"),
            "winerror": error.get("winerror"),
            "message": error.get("message"),
        },
        "validation": report.get("validation"),
        "cleanup": report.get("cleanup"),
        "leftovers_before": [entry["path"] for entry in report.get("leftovers_before", [])],
        "leftovers_after": [entry["path"] for entry in report.get("leftovers_after", [])],
        "runs": {
            label: [
                {
                    "pid": run["pid"],
                    "returncode": run["returncode"],
                    "elapsed_seconds": run["elapsed_seconds"],
                    "process_state_after": run["process_state_after"],
                }
                for run in runs
            ]
            for label, runs in report.get("runs", {}).items()
        },
    }
    (evidence_root / "summary.json").write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8",
    )
    print(json.dumps(summary, indent=2))


def diagnose(
    destination: Path, evidence_root: Path, target: str,
    observation_seconds: float, copy_experiment: bool,
) -> None:
    if os.name != "nt":
        raise ValueError("Windows publication diagnosis must run on native Windows")
    if target != "aarch64-pc-windows-msvc":
        raise ValueError("this diagnosis requires the native Windows ARM64 target")
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"diagnostic destination must not exist: {destination}")
    if evidence_root.exists() or evidence_root.is_symlink():
        raise ValueError(f"diagnostic evidence directory must not exist: {evidence_root}")
    evidence = destination.with_name(destination.name + "-evidence")
    if evidence.exists() or evidence.is_symlink():
        raise ValueError(f"compiler evidence destination must not exist: {evidence}")
    evidence_root.mkdir(parents=True)
    (evidence_root / "logs").mkdir()
    (evidence_root / "snapshots").mkdir()
    environment = compiler_environment()
    commit = PINS["source_commit"]
    pin = PINS["source"]
    bun_pin = PINS["bun"]["archives"][target]
    report = {
        "diagnostic": "windows-publication-ownership",
        "started_utc": utc_now(),
        "target": target,
        "destination": str(destination),
        "evidence_destination": str(evidence),
        "evidence_root": str(evidence_root),
        "python": {
            "version": sys.version,
            "executable": sys.executable,
            "pid": os.getpid(),
        },
        "platform": {
            "system": platform.system(),
            "machine": platform.machine(),
            "release": platform.release(),
            "version": platform.version(),
        },
        "child_environment_keys": sorted(environment),
        "credentials_forwarded_to_children": "GH_TOKEN" in environment,
        "mirrored_scripts": {
            "bootstrap_bend.py": file_sha256(Path(bootstrap_bend.__file__)),
            "bootstrap_bend_source.py": file_sha256(Path(bootstrap_bend_source.__file__)),
            "bend_compiler_pins.json": file_sha256(
                Path(bootstrap_bend.__file__).with_name("bend_compiler_pins.json"),
            ),
            "diagnose_windows_publication.py": file_sha256(Path(__file__)),
        },
        "source": {
            "commit": commit,
            "archive_size": pin["size"],
            "archive_sha256": pin["sha256"],
        },
        "bun": {
            "version": PINS["bun"]["version"],
            "revision": PINS["bun"]["revision"],
            "asset": bun_pin["asset"],
            "archive_size": bun_pin["size"],
            "archive_sha256": bun_pin["sha256"],
            "executable_size": bun_pin["executable"][0],
            "executable_sha256": bun_pin["executable"][1],
        },
        "runs": {},
        "snapshots": {},
        "lock_state": {},
        "publication": {},
    }
    root = None
    primary_error = None
    try:
        # Measure what the previous canonical attempt left behind before doing
        # anything that could take a scan lock of its own.
        report["leftovers_before"] = leftover_probe(destination.parent)
        report["defender_initial"] = defender_status(environment)
        report["snapshots"]["start"] = snapshot("start", environment, evidence_root)

        source = download(
            f"https://codeload.github.com/bendlang/bend/tar.gz/{commit}",
            pin["size"], pin["sha256"],
        )
        report["source"]["downloaded_size"] = len(source)
        report["source"]["downloaded_sha256"] = hashlib.sha256(source).hexdigest()
        destination.parent.mkdir(parents=True, exist_ok=True)
        root = Path(tempfile.mkdtemp(prefix="bend-source-", dir=destination.parent))
        report["staging_root"] = str(root)
        checkout = root / "source"
        extract_tar(source, checkout, f"bend-{commit}", {**pin["files"], **PINS["resources"]})
        adjust_windows_locator(checkout)
        report["locator"] = {
            "adjusted_size": (checkout / "bend2/bend.ts").stat().st_size,
            "adjusted_sha256": file_sha256(checkout / "bend2/bend.ts"),
            "original_sha256": pin["files"]["bend2/bend.ts"][1],
            "patched_sha256": pin["windows_locator"]["patched_file"][1],
        }
        bun = install_bun(root / "bun", target)
        _, revision_output, _ = run_recorded(
            [str(bun), "--revision"], label="bun_revision", cwd=root,
            environment=environment, timeout=60, capture=True, check=True,
            evidence_root=evidence_root, report=report,
        )
        revision = revision_output.strip()
        report["bun"]["observed_revision"] = revision
        if revision != f"{PINS['bun']['version']}+{PINS['bun']['revision'][:9]}":
            raise ValueError(f"unexpected pinned Bun revision: {revision}")
        _, host_output, _ = run_recorded(
            [str(bun), "-e", "console.log(process.platform + ':' + process.arch)"],
            label="bun_host", cwd=root, environment=environment, timeout=30,
            capture=True, check=True, evidence_root=evidence_root, report=report,
        )
        runtime_host = host_output.strip()
        report["bun"]["observed_host"] = runtime_host
        expected_host = "win32:arm64" if target == "aarch64-pc-windows-msvc" else "win32:x64"
        if runtime_host != expected_host:
            raise ValueError(
                f"Bun runtime host {runtime_host} does not match native target {target}",
            )
        distribution = root / "distribution"
        (distribution / "bin").mkdir(parents=True)
        (distribution / "bend2").mkdir()
        for name in ("base.bend", "bendtt.lean"):
            shutil.copyfile(checkout / "bend2" / name, distribution / "bend2" / name)
        shutil.copytree(checkout / "bend2/effs", distribution / "bend2/effs")
        shutil.copytree(checkout / "guide", distribution / "guide")
        output = distribution / "bin/bend.exe"
        # No --target: the verified native Bun embeds itself. An explicit
        # cross-target flag can download a different, unverified runtime.
        run_recorded(
            [str(bun), "build", "--compile", "bend2/main.ts", "--outfile", str(output)],
            label="bun_build", cwd=checkout, environment=environment, timeout=300,
            capture=False, check=True, evidence_root=evidence_root, report=report,
        )
        if output.is_symlink() or not output.is_file():
            raise ValueError("Bun did not produce a regular Bend executable")
        report["built"] = {
            "size": output.stat().st_size,
            "exclusive_after_build": exclusive_probe(output),
        }
        report["snapshots"]["after_build"] = snapshot(
            "after_build", environment, evidence_root,
        )
        observed = probe_identity_recorded(
            output, root, environment, evidence_root, report,
        )
        report["lock_state"]["after_identity_probe"] = exclusive_probe(output)
        # Production hashes the executable immediately after the identity
        # probe; keep that position and add a native PE receipt.
        executable_digest = file_sha256(output)
        report["built"]["sha256"] = executable_digest
        report["built"]["pe"] = pe_identity(output)
        report["snapshots"]["after_identity_probe"] = snapshot(
            "after_identity_probe", environment, evidence_root,
        )
        identity = {
            "bend_version": PINS["version"],
            "source_commit": commit,
            "source_archive_sha256": pin["sha256"],
            "original_locator_sha256": pin["files"]["bend2/bend.ts"][1],
            "adjusted_locator_sha256": pin["windows_locator"]["patched_file"][1],
            "bun_revision": revision,
            "bun_host": runtime_host,
            "bun_asset": bun_pin["asset"],
            "bun_archive_sha256": bun_pin["sha256"],
            "target": target,
            "executable_size": output.stat().st_size,
            "executable_sha256": executable_digest,
            "embedded_source_sha256": observed["fingerprint"],
            "embedded_entry": observed["entry"],
            "embedded_bun_version": observed["bun"],
            "embedded_bun_revision": observed["revision"],
            "embedded_host": observed["host"],
            "embedded_process": {
                key: observed[key] for key in ("exec_path", "argv0", "argv", "bun_main")
            },
            "validation": {"status": "pending"},
            "cli_checks": {},
        }
        identity_file = distribution / "compiler-build-identity.json"
        identity_file.write_text(json.dumps(identity, indent=2) + "\n", encoding="utf-8")
        try:
            if (
                observed["host"] != expected_host
                or observed["bun"] != PINS["bun"]["version"]
                or observed["revision"] != PINS["bun"]["revision"]
                or Path(observed["exec_path"]).resolve() != output.resolve()
            ):
                raise ValueError(f"unexpected embedded native compiler runtime: {observed}")
            # Both real CLI probes remain mandatory before installation. Never
            # use the preload to fake a successful version or resource check.
            for name, arguments in (("version", ["version"]), ("base", ["base", "--types"])):
                record, stdout, stderr = run_recorded(
                    [str(output), *arguments], label=f"cli_{name}", cwd=root,
                    environment=environment, timeout=30, capture=True, check=False,
                    evidence_root=evidence_root, report=report,
                )
                report["lock_state"][f"after_{name}"] = exclusive_probe(output)
                identity["cli_checks"][name] = {
                    "returncode": record["returncode"],
                    "stdout": stdout,
                    "stderr": stderr,
                }
                identity_file.write_text(
                    json.dumps(identity, indent=2) + "\n", encoding="utf-8",
                )
                if record["returncode"]:
                    raise subprocess.CalledProcessError(
                        record["returncode"], record["arguments"],
                        output=stdout, stderr=stderr,
                    )
                if name == "version" and stdout.strip() != f"bend {PINS['version']}":
                    raise ValueError(f"unexpected source-built Bend version: {stdout}")
        except BaseException as error:
            identity["validation"] = {"status": "failed", "error": str(error)}
            identity_file.write_text(
                json.dumps(identity, indent=2) + "\n", encoding="utf-8",
            )
            report["validation"] = {"status": "failed", "error": describe_exception(error)}
            report["publication"] = {"result": "not_attempted_validation_failed"}
            copy_if_present(identity_file, evidence_root / "compiler-build-identity.json")
            try:
                distribution.rename(evidence)
            except BaseException as move_error:
                report["publication"] = {
                    "result": "evidence_rename_failed",
                    "error": describe_exception(move_error),
                    "exclusive_output": exclusive_probe(output),
                    "holders": restart_manager_holders([output], environment),
                }
                raise
            raise
        identity["validation"] = {"status": "passed"}
        identity_file.write_text(json.dumps(identity, indent=2) + "\n", encoding="utf-8")
        report["validation"] = {"status": "passed"}
        copy_if_present(identity_file, evidence_root / "compiler-build-identity.json")
        copy_if_present(
            distribution / "compiler-embedded-source.js",
            evidence_root / "compiler-embedded-source.js",
        )
        (evidence_root / "identity.ts").write_text(PROBE_SOURCE, encoding="utf-8")
        # Publication is attempted immediately after the last real CLI probe,
        # exactly like production. Only cheap probes run in between.
        report["publication"] = {
            "destination": str(destination),
            "attempted_utc": utc_now(),
            "pre_rename": {
                "exclusive_output": exclusive_probe(output),
                "exclusive_distribution": exclusive_probe(distribution, directory=True),
                "control_exclusive_bun": exclusive_probe(bun),
                "control_exclusive_source": exclusive_probe(checkout / "bend2" / "main.ts"),
            },
        }
        try:
            distribution.rename(destination)
        except BaseException as error:
            report["publication"]["result"] = "failed"
            report["publication"]["failed_utc"] = utc_now()
            report["publication"]["error"] = describe_exception(error)
            report["publication"]["exclusive_after_failure"] = exclusive_probe(output)
            report["publication"]["holders"] = restart_manager_holders(
                [output], environment,
            )
            report["snapshots"]["publication_failure"] = snapshot(
                "publication_failure", environment, evidence_root,
            )
            report["publication"]["observation"] = bounded_lock_observation(
                output, observation_seconds,
            )
            report["publication"]["defender_after_failure"] = defender_status(environment)
            if copy_experiment:
                report["publication"]["copy_experiment"] = copy_publication_experiment(
                    distribution, destination, environment, evidence_root,
                )
            raise
        else:
            report["publication"]["result"] = "succeeded"
            report["publication"]["succeeded_utc"] = utc_now()
            report["snapshots"]["after_publication"] = snapshot(
                "after_publication", environment, evidence_root,
            )
    except BaseException as error:
        primary_error = error
        raise
    finally:
        cleanup_error = None
        if root is not None:
            try:
                shutil.rmtree(root)
                report["cleanup"] = {"path": str(root), "result": "removed"}
            except BaseException as error:
                cleanup_error = error
                report["cleanup"] = {
                    "path": str(root),
                    "result": "failed",
                    "error": describe_exception(error),
                }
        report["leftovers_after"] = leftover_probe(destination.parent)
        report["finished_utc"] = utc_now()
        write_report(report, evidence_root)
        if cleanup_error is not None and primary_error is None:
            raise cleanup_error


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--observation-seconds", type=float, default=10.0)
    parser.add_argument("--copy-experiment", action=argparse.BooleanOptionalAction, default=True)
    arguments = parser.parse_args()
    diagnose(
        arguments.destination.absolute(),
        arguments.evidence_dir.absolute(),
        arguments.target or native_target(),
        arguments.observation_seconds,
        arguments.copy_experiment,
    )


if __name__ == "__main__":
    main()
