#!/usr/bin/env python3
"""Provision checksum-pinned standalone bend-lint and native Bun for E2Es only."""

import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import shutil
import stat
import subprocess
import tarfile
import zipfile

from bootstrap_bend import compiler_environment, download, download_asset, native_target


# Primary release API: oven-sh/bun, bun-v1.4.4. Fixed asset IDs and digests
# bind mutable release names. The npm SHA256 was checked against registry SRI.
BUN = {
    "aarch64-apple-darwin": ("bun-darwin-aarch64", 665260236, 40323673, "c240d7f0ccb90c263a849fcdb09b7ee62630d921348ed6aafda86b9d577bc510"),
    "x86_64-apple-darwin": ("bun-darwin-x64", 665261039, 43442571, "50a3491f73abc2fe81673171a85762d3bc2cc1680ccecadd983de99adf77977e"),
    "aarch64-unknown-linux-gnu": ("bun-linux-aarch64", 665259460, 51644673, "baa6bb36f51d6e58561e02bbea825038a9df454f673d6d776e8bbf86f8c7850e"),
    "x86_64-unknown-linux-gnu": ("bun-linux-x64", 665259752, 51794595, "bf8ccbfb2b0b80159e0f9a6d15c55317add8077534dbc6130a3edb52fccb395c"),
    "aarch64-pc-windows-msvc": ("bun-windows-aarch64", 665259874, 49392935, "2d57a0eec0cb44d95c918c3f9734f60455a1306ae5085d76fc68f44e3ee30d09"),
    "x86_64-pc-windows-msvc": ("bun-windows-x64", 665265163, 54731706, "cd2c0fccc78f3ede1f50ca827a15f063eda13d4d150a3d0674f4f6e323d864a5"),
}
NPM_URL = "https://registry.npmjs.org/bend-lint/-/bend-lint-0.2.1.tgz"
NPM_SIZE = 86153
NPM_SHA256 = "9681def8595d09e19e13a385dd16e990b3320a22c54d3e3180dab4585124bb74"
# Registry SRI for that tarball; the SHA256 above was derived from the same bytes.
NPM_INTEGRITY = "sha512-Lf0/LXe/L8N2oVKrs6/foWBhlplJjNb3EV6g5NtfTgdF20p4mLLgEGFWlARc0DKvord0bRgDPuGtwqBvfOrAtA=="
# The published package imports exactly this rule API, and its config pin is
# bend-lint-rules 0.2.1.0. Both are verified below before provisioning succeeds.
RULE_API = "0.2.1.0"


def install_windows_clang(destination: Path) -> dict:
    # LLVM19-21 miscompile preserve_most on Windows ARM64 (llvm/llvm-project#166436);
    # LLVM22.1.8 fails the native rule program's SEH prologue. LLVM18 uses Bend's
    # existing plain-C calling-convention capability guard, keeping native rules.
    pin = {
        "asset": 176567333, "size": 345225147,
        "sha256": "e25bf44d67fe86708490cf08de085fe1d6e1e50f3249c212c9077a06247cdc9e",
    }
    extractor = shutil.which("7z")
    if extractor is None:
        raise ValueError("native LLVM provisioning requires the runner's 7-Zip")
    installer = destination.parent / "LLVM-18.1.8-woa64.exe"
    installer.write_bytes(download_asset("llvm/llvm-project", pin))
    try:
        # Extract the pinned NSIS payload without executing an installer,
        # changing system registry entries, or replacing the runner's LLVM.
        subprocess.run(
            [extractor, "x", "-y", f"-o{destination}", str(installer)],
            env=compiler_environment(), check=True, capture_output=True, timeout=180,
        )
    finally:
        installer.unlink()
    clang = destination / "bin/clang.exe"
    version = subprocess.check_output(
        [str(clang), "--version"], env=compiler_environment(), text=True, timeout=30,
    )
    target = subprocess.check_output(
        [str(clang), "-dumpmachine"], env=compiler_environment(), text=True, timeout=30,
    ).strip()
    if "clang version 18.1.8" not in version or target != "aarch64-pc-windows-msvc":
        raise ValueError(f"unexpected native clang identity: {version}; target={target}")
    attributes = subprocess.check_output(
        [str(clang), "-E", "-P", "-x", "c", "-"],
        input="__has_attribute(preserve_none) __has_attribute(preserve_most) __has_attribute(musttail)\n",
        env=compiler_environment(), text=True, timeout=30,
    ).split()
    if attributes != ["0", "1", "1"]:
        raise ValueError(f"unexpected native clang calling-convention capabilities: {attributes}")
    with clang.open("rb") as executable:
        checksum = hashlib.file_digest(executable, "sha256").hexdigest()
    return {
        "path": str(clang), "version_output": version, "target": target,
        "sha256": checksum, "release_asset": pin,
        "attributes": dict(zip(("preserve_none", "preserve_most", "musttail"), attributes, strict=True)),
    }


def extract_package(data: bytes, destination: Path) -> None:
    if destination.exists() or destination.is_symlink():
        raise ValueError("package destination must not exist")
    devices = {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(1, 10)), *(f"LPT{i}" for i in range(1, 10))}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        members = archive.getmembers()
        if len(members) > 4096 or sum(member.size for member in members) > 32 * 1024 * 1024:
            raise ValueError("unexpected package size")
        seen = {}
        for member in members:
            path = PurePosixPath(member.name)
            if (
                path.is_absolute() or ".." in path.parts or "\\" in member.name
                or not path.parts or path.parts[0] != "package"
                or not (member.isdir() or member.isfile()) or member.size < 0
                or member.mode & 0o7000 or str(path).casefold() in seen
                or any(":" in part or part.endswith((" ", ".")) or part.split(".")[0].upper() in devices for part in path.parts)
            ):
                raise ValueError(f"unsafe package member: {member.name}")
            seen[str(path).casefold()] = member
        for member in members:
            for parent in PurePosixPath(member.name).parents:
                ancestor = seen.get(str(parent).casefold())
                if ancestor is not None and not ancestor.isdir():
                    raise ValueError("non-directory package parent")
        destination.mkdir(parents=True)
        try:
            for member in members:
                relative = PurePosixPath(member.name).relative_to("package")
                output = destination.joinpath(*relative.parts)
                if member.isdir():
                    output.mkdir(parents=True, exist_ok=True)
                else:
                    output.parent.mkdir(parents=True, exist_ok=True)
                    source = archive.extractfile(member)
                    if source is None:
                        raise ValueError("unreadable package member")
                    with source, output.open("xb") as target:
                        shutil.copyfileobj(source, target)
        except BaseException:
            shutil.rmtree(destination)
            raise


def package_identity(package: Path) -> dict:
    metadata = json.loads((package / "package.json").read_text(encoding="utf-8"))
    if metadata["name"] != "bend-lint" or metadata["version"] != "0.2.1":
        raise ValueError("unexpected standalone linter identity")
    if (
        metadata.get("engines", {}).get("bun") != "1.4.4"
        or metadata.get("bendRelease") != "bendlang/bend/v2.0.39"
    ):
        raise ValueError("unexpected standalone linter runtime contract")
    script = package / "src/linter/seam.ts"
    defaults = package / "src/linter/defaults.json"
    cli = package / "src/linter/cli.bend"
    if not script.is_file() or not defaults.is_file() or not cli.is_file():
        raise ValueError("missing standalone linter entry points")
    rules = json.loads(defaults.read_text(encoding="utf-8"))
    if (
        not isinstance(rules, list)
        or not rules
        or not all(isinstance(rule, str) and rule.count("/") == 1 for rule in rules)
    ):
        raise ValueError("unexpected standard rule catalog")
    if f"bend-lint-api@{RULE_API}/" not in cli.read_text(encoding="utf-8"):
        raise ValueError("unexpected standalone linter rule API")
    return {
        "version": metadata["version"],
        "standard_rules": rules,
        "defaults_sha256": hashlib.sha256(defaults.read_bytes()).hexdigest(),
    }


def install(destination: Path, target: str) -> tuple[Path, Path]:
    if target != native_target() or target not in BUN:
        raise ValueError("requested linter runtime must match the native host")
    if destination.exists() or destination.is_symlink():
        raise ValueError("installation destination must not exist")
    name, asset, size, checksum = BUN[target]
    runtime_archive = download_asset("oven-sh/bun", {"asset": asset, "size": size, "sha256": checksum})
    package_archive = download(NPM_URL, NPM_SIZE, NPM_SHA256)
    executable_name = "bun.exe" if target.endswith("windows-msvc") else "bun"
    with zipfile.ZipFile(io.BytesIO(runtime_archive)) as archive:
        members = [entry for entry in archive.infolist() if entry.filename == f"{name}/{executable_name}"]
        if len(members) != 1 or members[0].is_dir() or members[0].file_size > 200 * 1024 * 1024:
            raise ValueError("missing or invalid native Bun executable")
        if stat.S_ISLNK(members[0].external_attr >> 16):
            raise ValueError("linked native Bun executable")
        executable_bytes = archive.read(members[0])
    destination.mkdir(parents=True)
    try:
        package = destination / "package"
        extract_package(package_archive, package)
        identity = package_identity(package)
        script = package / "src/linter/seam.ts"
        executable = destination / executable_name
        executable.write_bytes(executable_bytes)
        executable.chmod(0o755)
        observed = json.loads(subprocess.run(
            [str(executable), "-e", "console.log(JSON.stringify({version:Bun.version,platform:process.platform,arch:process.arch}))"],
            env=compiler_environment(), capture_output=True, text=True, check=True, timeout=30,
        ).stdout)
        platform = "darwin" if target.endswith("apple-darwin") else "win32" if target.endswith("windows-msvc") else "linux"
        expected = {"version": "1.4.4", "platform": platform, "arch": "arm64" if target.startswith("aarch64-") else "x64"}
        if observed != expected:
            raise ValueError(f"unexpected native Bun identity: {observed}")
        metadata = json.loads((package / "package.json").read_text(encoding="utf-8"))
        evidence = {
            "target": target, "runtime": observed, "runtime_asset": asset,
            "runtime_archive_sha256": checksum, "runtime_executable_sha256": hashlib.sha256(executable_bytes).hexdigest(),
            "linter_version": identity["version"], "npm_url": NPM_URL, "npm_sha256": NPM_SHA256,
            "npm_integrity": NPM_INTEGRITY, "rule_api": RULE_API,
            "standard_rules": identity["standard_rules"], "defaults_sha256": identity["defaults_sha256"],
            "upstream_bend_release": metadata["bendRelease"], "upstream_bend_windows": metadata["bendWindows"],
        }
        if target == "aarch64-pc-windows-msvc":
            evidence["native_clang"] = install_windows_clang(destination / "llvm-18.1.8")
        (destination / "lint-build-identity.json").write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    except BaseException:
        shutil.rmtree(destination)
        raise
    print(f"Provisioned standalone bend-lint 0.2.1 and native Bun 1.4.4 ({target})")
    return executable, script


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True, choices=BUN)
    parser.add_argument("--destination", required=True, type=Path)
    parser.add_argument("--github-env", type=Path)
    args = parser.parse_args()
    destination = args.destination.resolve()
    if any(character in str(destination) for character in "\r\n"):
        raise ValueError("installation path cannot contain line breaks")
    executable, script = install(destination, args.target)
    if args.github_env:
        with args.github_env.open("a", encoding="utf-8") as environment:
            environment.write(f"BEND2_LINT_TEST_PATH={executable}\n")
            environment.write(f"BEND2_LINT_TEST_ARGUMENTS={json.dumps([str(script)])}\n")
            identity = json.loads((destination / "lint-build-identity.json").read_text(encoding="utf-8"))
            if "native_clang" in identity:
                environment.write(f"CC={identity['native_clang']['path']}\n")


if __name__ == "__main__":
    main()
