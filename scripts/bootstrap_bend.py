#!/usr/bin/env python3
"""Provision checksum-pinned genuine Bend for native compiler E2Es on six targets."""

import argparse
import base64
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import platform
import shutil
import tarfile
import subprocess
import tempfile
import urllib.request


# Reviewed against the primary release APIs and downloaded bytes. Upstream
# releases are mutable; fixed asset IDs plus these digests bind their identity.
PINS = json.loads(Path(__file__).with_name("bend_compiler_pins.json").read_text())
TARGETS = (*PINS["archives"], *PINS["bun"]["archives"])


def native_target() -> str:
    system = platform.system()
    machine = platform.machine().lower()
    architecture = {"arm64": "aarch64", "aarch64": "aarch64", "x86_64": "x86_64", "amd64": "x86_64"}.get(machine)
    suffix = {"Darwin": "apple-darwin", "Linux": "unknown-linux-gnu", "Windows": "pc-windows-msvc"}.get(system)
    target = f"{architecture}-{suffix}"
    if target not in TARGETS:
        raise ValueError(f"unsupported native compiler host: {system} {machine}")
    return target


def download(url: str, size: int, checksum: str, *, authenticated: bool = False) -> bytes:
    request = urllib.request.Request(url, headers={"Accept": "application/octet-stream"})
    if authenticated and (token := os.environ.get("GH_TOKEN")):
        # Only fixed first-party API requests receive auth, never redirects.
        request.add_unredirected_header("Authorization", f"Bearer {token}")
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(size + 1)
    if len(data) != size or hashlib.sha256(data).hexdigest() != checksum:
        raise ValueError("archive size/checksum mismatch; refusing extraction")
    return data


def download_asset(repository: str, pin: dict) -> bytes:
    return download(
        f"https://api.github.com/repos/{repository}/releases/assets/{pin['asset']}",
        pin["size"], pin["sha256"], authenticated=True,
    )


def extract_tar(data: bytes, destination: Path, root: str, required_files: dict) -> None:
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"installation destination must not exist: {destination}")
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        members = archive.getmembers()
        seen = set()
        windows_seen = set()
        windows_devices = {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1, 10)), *(f"LPT{i}" for i in range(1, 10))}
        total = 0
        for member in members:
            path = PurePosixPath(member.name)
            if (
                path.is_absolute() or ".." in path.parts or "\\" in member.name
                or not path.parts or path.parts[0] != root
                or str(path) in seen or not (member.isdir() or member.isfile())
                or member.size < 0 or member.mode & 0o7000
            ):
                raise ValueError(f"unsafe archive member: {member.name}")
            seen.add(str(path))
            if os.name == "nt":
                windows_path = str(path).casefold()
                if windows_path in windows_seen or any(
                    ":" in part or part.endswith((" ", "."))
                    or part.split(".")[0].upper() in windows_devices
                    for part in path.parts
                ):
                    raise ValueError(f"unsafe Windows archive member: {member.name}")
                windows_seen.add(windows_path)
            total += member.size
        if total > 128 * 1024 * 1024:
            raise ValueError("unexpected unpacked size")
        by_name = {str(PurePosixPath(member.name)): member for member in members}
        for relative, (size, checksum) in required_files.items():
            required = f"{root}/{relative}"
            member = by_name.get(required)
            if member is None or not member.isfile() or member.size != size:
                raise ValueError(f"missing or invalid archive member: {required}")
            source = archive.extractfile(member)
            if source is None:
                raise ValueError(f"unreadable archive member: {required}")
            with source:
                if hashlib.file_digest(source, "sha256").hexdigest() != checksum:
                    raise ValueError(f"archive member checksum mismatch: {required}")
        if "bin/bend" in required_files and by_name[f"{root}/bin/bend"].mode != 0o755:
            raise ValueError("Bend executable metadata is invalid")
        effects = by_name.get(f"{root}/bend2/effs")
        if effects is None or not effects.isdir():
            raise ValueError("missing Bend effect resources")
        # Validate every parent before making files; never extract links,
        # ownership, or archive permission bits.
        for member in members:
            for parent in PurePosixPath(member.name).parents:
                if str(parent) in by_name and not by_name[str(parent)].isdir():
                    raise ValueError(f"non-directory archive parent: {parent}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.mkdir()
        try:
            for member in members:
                relative = PurePosixPath(member.name).relative_to(root)
                output = destination.joinpath(*relative.parts)
                if member.isdir():
                    output.mkdir(parents=True, exist_ok=True)
                else:
                    output.parent.mkdir(parents=True, exist_ok=True)
                    source = archive.extractfile(member)
                    if source is None:
                        raise ValueError(f"unreadable archive member: {member.name}")
                    with source, output.open("xb") as target:
                        shutil.copyfileobj(source, target)
                    output.chmod(0o755 if relative == PurePosixPath("bin/bend") else 0o644)
        except BaseException:
            shutil.rmtree(destination)
            raise


def compiler_environment() -> dict:
    # Keep read-only download credentials and CI/cache secrets out of every
    # compiler/runtime subprocess, including authenticated Unix provisioning.
    operating_system_environment = {
        "PATH", "PATHEXT", "SYSTEMROOT", "SYSTEMDRIVE", "WINDIR", "COMSPEC",
        "TEMP", "TMP", "TMPDIR", "HOME", "USERPROFILE", "HOMEDRIVE",
        "HOMEPATH", "APPDATA", "LOCALAPPDATA", "PROGRAMFILES",
        "PROGRAMFILES(X86)", "PROGRAMW6432", "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
    }
    return {
        **{key: value for key, value in os.environ.items() if key.upper() in operating_system_environment},
        "BEND_NO_TELEMETRY": "1",
    }


def probe_compiler_identity(output: Path, staging: Path, environment: dict) -> dict:
    probe = staging / "identity.ts"
    result = staging / "embedded-identity.json"
    source_output = output.parent.parent / "compiler-embedded-source.js"
    probe.write_text(
        "import { createHash } from 'node:crypto';\n"
        "import { readFileSync, writeFileSync } from 'node:fs';\n"
        "const entry = process.argv[1];\n"
        "if (!/^(?:\\/\\$bunfs\\/|B:[\\\\/]~BUN[\\\\/])/.test(entry ?? '')) {\n"
        "  throw new Error('Expected a compiled Bun virtual entry');\n"
        "}\n"
        "const source = readFileSync(entry);\n"
        "writeFileSync(process.env.BEND2_COMPILER_SOURCE_OUTPUT, source, { flag: 'wx' });\n"
        "writeFileSync(process.env.BEND2_COMPILER_IDENTITY_OUTPUT, JSON.stringify({\n"
        "  fingerprint: createHash('sha256').update(source).digest('hex'),\n"
        "  bun: Bun.version, revision: Bun.revision,\n"
        "  host: process.platform + ':' + process.arch, entry,\n"
        "}), { flag: 'wx' });\n"
        "process.exit(0);\n",
        encoding="utf-8",
    )
    # Bun's BUN_OPTIONS parser silently ignores quoted preload paths, while
    # direct file URLs mishandle percent-encoded spaces. Import the encoded
    # file URL from a whitespace-free inline module, without changing cwd.
    module = f"await import({json.dumps(probe.as_uri())});"
    preload = base64.b64encode(module.encode("utf-8")).decode("ascii")
    probe_environment = {
        **environment,
        "BUN_OPTIONS": f"--preload data:text/javascript;base64,{preload}",
        "BEND2_COMPILER_IDENTITY_OUTPUT": str(result),
        "BEND2_COMPILER_SOURCE_OUTPUT": str(source_output),
    }
    subprocess.run(
        [str(output), "version"], cwd=staging, env=probe_environment,
        check=True, timeout=30,
    )
    observed = json.loads(result.read_text(encoding="utf-8"))
    with source_output.open("rb") as source:
        fingerprint = hashlib.file_digest(source, "sha256").hexdigest()
    if observed["fingerprint"] != fingerprint:
        raise ValueError("embedded compiler source identity mismatch")
    return observed


def install(destination: Path, target: str, version: str) -> Path:
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"installation destination must not exist: {destination}")
    if version == PINS["legacy"]["version"]:
        pin = PINS["legacy"]
        if target != pin["target"]:
            raise ValueError("legacy Bend verification is macOS ARM64 only")
        required = pin["files"]
    elif target in PINS["archives"]:
        pin = PINS["archives"][target]
        required = {**PINS["resources"], "bin/bend": pin["executable"]}
    else:
        from bootstrap_bend_source import build_windows
        return build_windows(destination, target)
    extract_tar(download_asset("bendlang/bend", pin), destination, "bend", required)
    executable = destination / "bin/bend"
    try:
        with tempfile.TemporaryDirectory(prefix="bend-identity-", dir=destination.parent) as temporary:
            observed = probe_compiler_identity(executable, Path(temporary), compiler_environment())
        host = "darwin" if target.endswith("apple-darwin") else "linux"
        architecture = "arm64" if target.startswith("aarch64-") else "x64"
        runtime = pin.get("runtime", PINS["bun"])
        if (
            observed["host"] != f"{host}:{architecture}"
            or observed["bun"] != runtime["version"]
            or observed["revision"] != runtime["revision"]
        ):
            raise ValueError(f"unexpected official compiler embedded runtime: {observed}")
        identity = {
            "bend_version": version,
            "source_commit": pin.get("source_commit", PINS["source_commit"]),
            "release_asset": pin["asset"],
            "archive_size": pin["size"],
            "archive_sha256": pin["sha256"],
            "target": target,
            "executable_size": required["bin/bend"][0],
            "executable_sha256": required["bin/bend"][1],
            "embedded_source_sha256": observed["fingerprint"],
            "embedded_entry": observed["entry"],
            "embedded_bun_version": observed["bun"],
            "embedded_bun_revision": observed["revision"],
            "embedded_host": observed["host"],
        }
        (destination / "compiler-build-identity.json").write_text(
            json.dumps(identity, indent=2) + "\n", encoding="utf-8",
        )
    except BaseException:
        shutil.rmtree(destination)
        raise
    print(f"Installed official Bend {version} ({target}, {pin['sha256']}): {executable}")
    return executable


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True)
    # Explicit target comes from the workflow's verified native Rust host;
    # Python itself can otherwise be emulated on Windows ARM64.
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--version", choices=(PINS["version"], PINS["legacy"]["version"]), default=PINS["version"])
    parser.add_argument("--github-env", type=Path)
    arguments = parser.parse_args()
    executable = install(arguments.destination.absolute(), arguments.target or native_target(), arguments.version)
    if arguments.github_env:
        with arguments.github_env.open("a", encoding="utf-8") as environment:
            environment.write(f"BEND2_COMPILER_TEST_PATH={executable}\nBEND_NO_TELEMETRY=1\n")


if __name__ == "__main__":
    main()
