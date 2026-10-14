#!/usr/bin/env python3
"""Build the exact genuine Bend source with a verified native Windows Bun runtime."""

import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import zipfile

from bootstrap_bend import PINS, compiler_environment, download, download_asset, extract_tar, probe_compiler_identity


def install_bun(destination: Path, target: str) -> Path:
    pin = PINS["bun"]["archives"][target]
    data = download_asset("oven-sh/bun", pin)
    root = pin["name"]
    with zipfile.ZipFile(io.BytesIO(data)) as archive:
        members = archive.infolist()
        names = [member.filename for member in members]
        if sorted(names) != [f"{root}/", f"{root}/bun.exe"]:
            raise ValueError("unexpected Bun archive contents")
        directory = archive.getinfo(f"{root}/")
        executable = archive.getinfo(f"{root}/bun.exe")
        if (
            not directory.is_dir() or directory.file_size != 0
            or stat.S_IFMT(directory.external_attr >> 16) != stat.S_IFDIR
            or executable.is_dir()
            or stat.S_IFMT(executable.external_attr >> 16) != stat.S_IFREG
            or executable.flag_bits & 1
            or executable.file_size != pin["executable"][0]
        ):
            raise ValueError("unsafe Bun archive member metadata")
        content = archive.read(executable)
        if hashlib.sha256(content).hexdigest() != pin["executable"][1]:
            raise ValueError("Bun executable checksum mismatch")
    destination.mkdir()
    output = destination / "bun.exe"
    with output.open("xb") as installed:
        installed.write(content)
    return output


def adjust_windows_locator(checkout: Path) -> None:
    # Identify compiled modules by Bun's native virtual entry (argv[1]).
    # URL spelling differs across platforms; keep adjacent physical executable
    # resources and the original source branch, without touching the kernel.
    source = checkout / "bend2/bend.ts"
    content = source.read_bytes()
    original_size, original_digest = PINS["source"]["files"]["bend2/bend.ts"]
    if len(content) != original_size or hashlib.sha256(content).hexdigest() != original_digest:
        raise ValueError("unexpected original Bend resource-locator source")
    locator = PINS["source"]["windows_locator"]
    anchor = locator["anchor"].encode("utf-8")
    if content.count(anchor) != 1:
        raise ValueError("Bend resource-locator anchor must occur exactly once")
    adjusted = content.replace(anchor, locator["replacement"].encode("utf-8"))
    patched_size, patched_digest = locator["patched_file"]
    if len(adjusted) != patched_size or hashlib.sha256(adjusted).hexdigest() != patched_digest:
        raise ValueError("unexpected adjusted Bend resource-locator source")
    source.write_bytes(adjusted)


def build_windows(destination: Path, target: str) -> Path:
    if os.name != "nt":
        raise ValueError("Windows compiler source bootstrap must run on a native Windows runner")
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"installation destination must not exist: {destination}")
    evidence = destination.with_name(destination.name + "-evidence")
    if evidence.exists() or evidence.is_symlink():
        raise ValueError(f"compiler evidence destination must not exist: {evidence}")
    commit = PINS["source_commit"]
    pin = PINS["source"]
    source = download(
        f"https://codeload.github.com/bendlang/bend/tar.gz/{commit}",
        pin["size"], pin["sha256"],
    )
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="bend-source-", dir=destination.parent) as temporary:
        root = Path(temporary)
        checkout = root / "source"
        extract_tar(source, checkout, f"bend-{commit}", {**pin["files"], **PINS["resources"]})
        adjust_windows_locator(checkout)
        bun = install_bun(root / "bun", target)
        # Credentials are scoped only to the fixed download requests, never the
        # reviewed source's subprocesses. Build has no package install/network
        # resolution: upstream CLI imports only local TS and built-in modules.
        environment = compiler_environment()
        revision = subprocess.check_output([str(bun), "--revision"], cwd=root, env=environment, text=True).strip()
        if revision != f"{PINS['bun']['version']}+{PINS['bun']['revision'][:9]}":
            raise ValueError(f"unexpected pinned Bun revision: {revision}")
        runtime_host = subprocess.check_output([
            str(bun), "-e", "console.log(process.platform + ':' + process.arch)",
        ], cwd=root, env=environment, text=True, timeout=30).strip()
        expected_host = "win32:arm64" if target == "aarch64-pc-windows-msvc" else "win32:x64"
        if runtime_host != expected_host:
            raise ValueError(f"Bun runtime host {runtime_host} does not match native target {target}")
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
        subprocess.run([
            str(bun), "build", "--compile",
            "bend2/main.ts", "--outfile", str(output),
        ], cwd=checkout, env=environment, check=True, timeout=300)
        if output.is_symlink() or not output.is_file():
            raise ValueError("Bun did not produce a regular Bend executable")
        # Capture native source/runtime/process identity before ordinary CLI
        # initialization, so a real resource failure still has reviewable evidence.
        observed = probe_compiler_identity(output, root, environment)
        with output.open("rb") as executable:
            executable_digest = hashlib.file_digest(executable, "sha256").hexdigest()
        identity = {
            "bend_version": PINS["version"],
            "source_commit": commit,
            "source_archive_sha256": pin["sha256"],
            "original_locator_sha256": pin["files"]["bend2/bend.ts"][1],
            "adjusted_locator_sha256": pin["windows_locator"]["patched_file"][1],
            "bun_revision": revision,
            "bun_host": runtime_host,
            "bun_asset": PINS["bun"]["archives"][target]["asset"],
            "bun_archive_sha256": PINS["bun"]["archives"][target]["sha256"],
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
                completed = subprocess.run(
                    [str(output), *arguments], cwd=root, env=environment,
                    capture_output=True, text=True, timeout=30,
                )
                identity["cli_checks"][name] = {
                    "returncode": completed.returncode,
                    "stdout": completed.stdout,
                    "stderr": completed.stderr,
                }
                identity_file.write_text(json.dumps(identity, indent=2) + "\n", encoding="utf-8")
                completed.check_returncode()
                if name == "version" and completed.stdout.strip() != f"bend {PINS['version']}":
                    raise ValueError(f"unexpected source-built Bend version: {completed.stdout}")
        except BaseException as error:
            identity["validation"] = {"status": "failed", "error": str(error)}
            identity_file.write_text(json.dumps(identity, indent=2) + "\n", encoding="utf-8")
            # This is evidence, not an installation. The failed compiler is
            # never exposed through BEND2_COMPILER_TEST_PATH.
            distribution.rename(evidence)
            raise
        identity["validation"] = {"status": "passed"}
        identity_file.write_text(json.dumps(identity, indent=2) + "\n", encoding="utf-8")
        distribution.rename(destination)
    executable = destination / "bin/bend.exe"
    print(f"Built genuine Bend {PINS['version']} source {commit} with native {target} Bun {revision}: {executable}")
    return executable
