#!/usr/bin/env python3
"""Capture a failed genuine Windows ARM analyzer without changing its rules."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess

from bootstrap_bend import compiler_environment, native_target


SDK_VERSION = "10.1.26100.8249"


def pe_identity(path: Path) -> dict:
    with path.open("rb") as binary:
        if binary.read(2) != b"MZ":
            raise ValueError(f"not a PE executable: {path}")
        binary.seek(60)
        offset = struct.unpack("<I", binary.read(4))[0]
        binary.seek(offset)
        if binary.read(4) != b"PE\0\0":
            raise ValueError(f"missing PE signature: {path}")
        coff = binary.read(20)
        machine = struct.unpack_from("<H", coff)[0]
        optional = binary.read(struct.unpack_from("<H", coff, 16)[0])
        if machine not in (0xAA64, 0xA641) or struct.unpack_from("<H", optional)[0] != 0x20B:
            raise ValueError(f"not a native ARM64 PE32+ executable: {path}")
        stack_reserve, stack_commit = struct.unpack_from("<QQ", optional, 72)
    with path.open("rb") as binary:
        digest = hashlib.file_digest(binary, "sha256").hexdigest()
    return {
        "path": str(path), "sha256": digest, "machine": hex(machine),
        "stack_reserve_bytes": stack_reserve, "stack_commit_bytes": stack_commit,
    }


def provision_debugger(environment: dict, output: Path) -> dict:
    # Reuse the exact Microsoft-signed SDK bundle already cached by this runner
    # image. Never select the latest SDK, use winget, or follow a moving fwlink.
    script = r'''
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$version = '10.1.26100.8249'
$entries = @(Get-ItemProperty 'HKLM:\Software\Wow6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*' |
    Where-Object { $_.PSObject.Properties['DisplayName'] -and $_.PSObject.Properties['DisplayVersion'] -and $_.PSObject.Properties['BundleCachePath'] -and $_.DisplayName -match 'Windows Software Development Kit' -and $_.DisplayVersion -eq $version })
$paths = @($entries | ForEach-Object { $_.BundleCachePath } | Sort-Object -Unique)
if ($paths.Count -ne 1) { throw "Expected exactly one cached SDK $version bundle" }
$installer = $paths[0]
$signature = Get-AuthenticodeSignature -LiteralPath $installer
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation(?:,|$)') { throw 'SDK bundle signature is not valid Microsoft code signing' }
$installerHash = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash.ToLowerInvariant()
$process = Start-Process -FilePath $installer -ArgumentList '/features','OptionId.WindowsDesktopDebuggers','/q','/norestart' -PassThru -Wait
if ($process.ExitCode -notin @(0,3010)) { throw "SDK debugger installation exited $($process.ExitCode)" }
$cdb = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\Debuggers\arm64\cdb.exe'
if (-not (Test-Path -LiteralPath $cdb -PathType Leaf)) { throw 'Native ARM64 cdb was not installed' }
$cdbSignature = Get-AuthenticodeSignature -LiteralPath $cdb
if ($cdbSignature.Status -ne 'Valid' -or $cdbSignature.SignerCertificate.Subject -notmatch 'O=Microsoft Corporation(?:,|$)') { throw 'cdb signature is not valid Microsoft code signing' }
@{sdk_version=$version; installer=$installer; installer_sha256=$installerHash; signer=$signature.SignerCertificate.Subject; cdb=$cdb; cdb_file_version=(Get-Item -LiteralPath $cdb).VersionInfo.FileVersion} | ConvertTo-Json -Compress
'''
    completed = subprocess.run(
        ["pwsh", "-NoProfile", "-NonInteractive", "-Command", script],
        env=environment, capture_output=True, text=True, timeout=360,
    )
    (output / "debugger-install.stdout").write_text(completed.stdout, encoding="utf-8")
    (output / "debugger-install.stderr").write_text(completed.stderr, encoding="utf-8")
    completed.check_returncode()
    identity = json.loads(completed.stdout)
    if identity["sdk_version"] != SDK_VERSION:
        raise ValueError("unexpected debugger SDK version")
    identity["pe"] = pe_identity(Path(identity["cdb"]))
    return identity


def diagnose(output: Path) -> None:
    if native_target() != "aarch64-pc-windows-msvc":
        raise ValueError("this diagnostic requires a native Windows ARM64 runner")
    replay = json.loads((output / "replay.json").read_text(encoding="utf-8"))
    executable = Path(replay["executable"])
    entry = Path(replay["entry"])
    project = Path(replay["cwd"])
    retained = Path(replay["retained_root"])
    if not all(path.is_absolute() and path.exists() for path in (executable, entry, project, retained)):
        raise ValueError("the original failed invocation has not been retained")
    if not entry.resolve().is_relative_to(retained.resolve()) or entry.parent != project:
        raise ValueError("the failed input escapes its retained workspace")
    if executable != Path(os.environ["BEND2_LINT_TEST_PATH"]) or replay["arguments"] != json.loads(os.environ["BEND2_LINT_TEST_ARGUMENTS"]):
        raise ValueError("replay differs from the configured genuine analyzer")
    environment = compiler_environment()
    environment["CC"] = os.environ["CC"]
    clang = Path(environment["CC"])
    identity = {"compiler": pe_identity(clang), "programs": [], "environment_keys": sorted(environment)}
    programs = Path(environment["LOCALAPPDATA"]) / "bend-lint" / "server" / "programs"
    try:
        for program in sorted(programs.glob("*.exe")):
            item = pe_identity(program)
            destination = output / f"program-{item['sha256']}.exe"
            shutil.copyfile(program, destination)
            with (destination.with_suffix(".headers.txt")).open("w", encoding="utf-8") as headers:
                subprocess.run(
                    [str(clang.parent / "llvm-readobj.exe"), "--file-headers", "--unwind", str(program)],
                    env=environment, stdout=headers, stderr=subprocess.STDOUT, check=True, timeout=60,
                )
            identity["programs"].append(item)
        if not identity["programs"]:
            raise ValueError("no compiled genuine rule program was retained")
        debugger = provision_debugger(environment, output)
        identity["debugger"] = debugger
        dump = project / "native-fault.dmp"
        if dump.exists() or (output / dump.name).exists():
            raise ValueError("refusing to reuse a previous exception dump")
        commands = output / "native-fault.cdb"
        capture = ".lastevent; .exr -1; .ecxr; r; kv; .dump /m native-fault.dmp; q"
        # -c2 runs only for an unhandled second chance. The runtime deliberately
        # handles first-chance AVs when committing its reserved memory pools.
        commands.write_text(
            f'.childdbg 1\nsxd -c2 "{capture}" *\n'
            f'sxd -c2 "{capture}" av\nsxi ibp\n'
            f'sxe -c "{capture}" sov\n.echo ==== ARMED ====\ng\n',
            encoding="utf-8",
        )
        command = [
            debugger["cdb"], "-o", "-cf", str(commands), str(executable),
            *replay["arguments"], str(entry), "--json", "--jobs", "1",
        ]
        identity["debugger_command"] = command
        # Only OS/runtime variables and the pinned CC reach the debuggee. In
        # particular, CI, artifact, repository, and cache credentials must never
        # enter a minidump. /m retains thread contexts/stacks, not the full heap.
        with (output / "native-fault.log").open("w", encoding="utf-8") as log:
            process = subprocess.Popen(
                command, cwd=project, env=environment, stdout=log, stderr=subprocess.STDOUT,
            )
            try:
                identity["debugger_exit"] = process.wait(timeout=180)
            except subprocess.TimeoutExpired:
                subprocess.run(
                    ["taskkill", "/PID", str(process.pid), "/T", "/F"],
                    env=environment, check=False, capture_output=True, timeout=30,
                )
                process.wait(timeout=30)
                raise
        if not dump.is_file():
            raise ValueError("cdb produced no exception dump; inspect the retained debugger log")
        with dump.open("rb") as recorded:
            identity["minidump_sha256"] = hashlib.file_digest(recorded, "sha256").hexdigest()
        shutil.move(str(dump), output / dump.name)
    except Exception as error:
        identity["diagnostic_error"] = str(error)
        raise
    finally:
        (output / "native-fault-identity.json").write_text(
            json.dumps(identity, indent=2, sort_keys=True) + "\n", encoding="utf-8",
        )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    arguments = parser.parse_args()
    diagnose(arguments.evidence_dir.resolve(strict=True))


if __name__ == "__main__":
    main()
