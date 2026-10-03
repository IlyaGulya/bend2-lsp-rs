#!/usr/bin/env python3
"""Generate cargo-dist installers without rebuilding native-tested executables."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib

DIST_VERSION = "0.33.0"
INSTALLERS = ("bend2-lsp-installer.sh", "bend2-lsp-installer.ps1")
METADATA = "installers.metadata.json"


def validate_installers(tag, version):
    from release import TARGETS, archive_name, file_sha256

    dist = Path("dist")
    expected = {METADATA}
    public = set()
    checksums = []
    digests = {}
    for name in INSTALLERS:
        expected.update((name, f"{name}.sha256"))
        public.update((name, f"{name}.sha256"))
        for path in (dist / name, dist / f"{name}.sha256"):
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"installer artifact must be a regular file: {path.name}")
        digest = file_sha256(dist / name)
        checksum = f"{digest}  {name}\n"
        if (dist / f"{name}.sha256").read_text() != checksum:
            raise ValueError(f"installer checksum mismatch: {name}")
        digests[name] = digest
        checksums.append(checksum)
    metadata = dist / METADATA
    if metadata.is_symlink() or not metadata.is_file():
        raise ValueError("installer metadata must be a regular file")
    if json.loads(metadata.read_text()) != {
        "source_sha": os.environ["RELEASE_SHA"],
        "tag": tag,
        "version": version,
        "channel": os.environ["RELEASE_CHANNEL"],
        "cargo_dist_version": DIST_VERSION,
        "assets": digests,
        "archives": {archive_name(target): file_sha256(dist / archive_name(target)) for target in TARGETS},
    }:
        raise ValueError("installer identity mismatch")
    return expected, public, checksums


def powershell_checksums(script, checksums):
    # cargo-dist 0.33.0 does not verify PowerShell downloads. Keep its installer,
    # but insert a fail-closed SHA256 check before its single archive extraction.
    anchor = "  Invoke-DownloadFile -client $wc -url $url -path $dir_path\n"
    if script.count(anchor) != 1:
        raise ValueError("pinned PowerShell download hook changed")
    entries = "\n".join(f'    "{name}" = "{digest}"' for name, digest in sorted(checksums.items()))
    guard = (
        "  $archive_checksums = @{\n" + entries + "\n  }\n"
        "  $expected_sha256 = $archive_checksums[$artifact_name]\n"
        "  $actual_sha256 = (Get-FileHash -LiteralPath $dir_path -Algorithm SHA256).Hash.ToLowerInvariant()\n"
        '  if (-not $expected_sha256 -or $actual_sha256 -ne $expected_sha256) {\n'
        '    throw "ERROR: archive checksum mismatch: $artifact_name"\n'
        "  }\n"
    )
    return script.replace(anchor, anchor + guard)


def generate():
    from release import TARGETS, archive_name, file_sha256, validate_artifacts, verify

    version = verify()
    tag = os.environ["RELEASE_TAG"]
    root = Path.cwd()
    dist = root / "dist"
    validate_artifacts(tag, version, include_installers=False)
    generator = os.environ.get("DIST_BIN", "dist")
    if subprocess.check_output([generator, "--version"], text=True).strip() != f"cargo-dist {DIST_VERSION}":
        raise ValueError("cargo-dist generator version mismatch")
    config = (root / "dist-workspace.toml").read_text()
    parsed = tomllib.loads(config)["dist"]
    if parsed["cargo-dist-version"] != DIST_VERSION or tuple(parsed["targets"]) != TARGETS:
        raise ValueError("cargo-dist configuration does not match native release targets")
    archives = {archive_name(target): file_sha256(dist / archive_name(target)) for target in TARGETS}
    url = parsed["simple-download-url"].replace("{tag}", tag)
    config = config.replace(json.dumps(parsed["simple-download-url"]), json.dumps(url))
    # Supported generic-project metadata lets global generation use a per-release
    # URL without editing committed files or rebuilding the tested Rust binaries.
    package = (
        '\n[package]\nname = "bend2-lsp"\n'
        f"version = {json.dumps(version)}\n"
        'repository = "https://github.com/IlyaGulya/bend2-lsp-rs"\n'
        'binaries = ["bend2-lsp"]\n'
        "build-command = " + json.dumps([
            "cargo", "build", "--manifest-path", str(root / "Cargo.toml"),
            "--locked", "--release", "--bin", "bend2-lsp",
        ]) + "\n"
    )
    with tempfile.TemporaryDirectory(prefix="bend2-dist-global-") as temporary:
        workspace = Path(temporary)
        (workspace / "dist-workspace.toml").write_text(config.replace('members = ["cargo:."]', 'members = ["dist:."]'))
        (workspace / "dist.toml").write_text(package)
        arguments = ["--tag", f"v{version}", "--output-format=json"]
        planned = json.loads(subprocess.check_output([
            generator, "manifest", "--artifacts=all", *arguments,
        ], cwd=workspace, text=True))
        output = workspace / "target/distrib"
        output.mkdir(parents=True, exist_ok=True)
        for name, digest in archives.items():
            planned["artifacts"][name]["checksums"] = {"sha256": digest}
        (output / "native-dist-manifest.json").write_text(json.dumps(planned))
        generated = json.loads(subprocess.check_output([
            generator, "build", "--artifacts=global", *arguments,
        ], cwd=workspace, text=True))
        if generated["announcement_tag"] != f"v{version}":
            raise ValueError("cargo-dist announcement identity mismatch")
        for name in INSTALLERS:
            script = (output / name).read_text()
            if tag != f"v{version}":
                # GitHub hosting is required by upstream receipts, even when
                # simple hosting owns the real URL. Bind its two mirror routes
                # and fallback route to the same immutable nightly, not stable.
                route = f"/releases/download/v{version}"
                if script.count(route) != 3:
                    raise ValueError("pinned GitHub installer download routes changed")
                script = script.replace(route, f"/releases/download/{tag}")
            if name.endswith(".ps1"):
                script = powershell_checksums(script, archives)
            (dist / name).write_text(script, encoding="utf-8", newline="\n")
            (dist / name).chmod(0o755 if name.endswith(".sh") else 0o644)
    digests = {name: file_sha256(dist / name) for name in INSTALLERS}
    for name, digest in digests.items():
        (dist / f"{name}.sha256").write_text(f"{digest}  {name}\n", encoding="utf-8", newline="\n")
    (dist / METADATA).write_text(json.dumps({
        "source_sha": os.environ["RELEASE_SHA"], "tag": tag, "version": version,
        "channel": os.environ["RELEASE_CHANNEL"], "cargo_dist_version": DIST_VERSION,
        "assets": digests, "archives": archives,
    }, indent=2) + "\n", encoding="utf-8", newline="\n")
    validate_artifacts(tag, version)
    print(f"Generated checksum-bound Shell and PowerShell installers for {tag}")


if __name__ == "__main__":
    generate()
