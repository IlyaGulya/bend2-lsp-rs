#!/usr/bin/env python3
"""Package native binaries and publish only complete, SHA-verified releases."""

import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tomllib
import urllib.error
import urllib.request
import zipfile

TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)


def api(endpoint, method="GET", data=None, missing_ok=False):
    repository = os.environ["GITHUB_REPOSITORY"]
    base = os.environ.get("GITHUB_API_URL", "https://api.github.com")
    request = urllib.request.Request(
        f"{base}/repos/{repository}/{endpoint}",
        data=None if data is None else json.dumps(data).encode(),
        method=method,
        headers={
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "Content-Type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        if missing_ok and error.code == 404:
            return None
        raise


def source_version():
    with Path("Cargo.toml").open("rb") as source:
        return tomllib.load(source)["package"]["version"]


def verify():
    sha = os.environ["RELEASE_SHA"]
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("release source must be a full commit SHA")
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
    if head != sha:
        raise ValueError("checkout does not match the quality-tested SHA")
    version = source_version()
    manifest = json.loads(Path(".release-please-manifest.json").read_text())
    with Path("Cargo.lock").open("rb") as lock_file:
        lock = tomllib.load(lock_file)
    versions = [item["version"] for item in lock["package"] if item["name"] == "bend2-lsp"]
    if manifest["."] != version or versions != [version]:
        raise ValueError("Cargo.toml, Cargo.lock and release manifest versions disagree")
    if os.environ["RELEASE_CHANNEL"] == "stable":
        tag = os.environ.get("RELEASE_TAG", f"v{version}")
        if tag != f"v{version}" or not re.fullmatch(r"\d+\.\d+\.\d+", version):
            raise ValueError("stable tag must match the non-prerelease Cargo version")
    elif os.environ["RELEASE_CHANNEL"] == "nightly":
        if not re.fullmatch(rf"nightly-\d{{4}}-\d{{2}}-\d{{2}}-{sha}", os.environ["RELEASE_TAG"]):
            raise ValueError("nightly tag must contain the date and exact source SHA")
    else:
        raise ValueError("unknown release channel")
    return version


def tag_sha(tag, missing_ok=False):
    ref = api(f"git/ref/tags/{tag}", missing_ok=missing_ok)
    if ref is None:
        return None
    obj = ref["object"]
    for _ in range(8):
        if obj["type"] == "commit":
            return obj["sha"]
        if obj["type"] != "tag":
            raise ValueError("release tag does not reference a commit")
        obj = api(f"git/tags/{obj['sha']}")["object"]
    raise ValueError("release tag nesting exceeds supported depth")


def release_for_tag(tag):
    # Draft releases are included for an authenticated token with contents access.
    # Pagination also recovers drafts after release-please's outputs disappear on retry.
    page = 1
    while True:
        releases = api(f"releases?per_page=100&page={page}")
        for release in releases:
            if release["tag_name"] == tag:
                return release
        if len(releases) < 100:
            return None
        page += 1


def candidate():
    version = verify()
    tag = f"v{version}"
    release = release_for_tag(tag)
    publish = False
    if release is not None:
        actual_sha = tag_sha(tag)
        if actual_sha == os.environ["RELEASE_SHA"]:
            if release["prerelease"]:
                raise ValueError("stable release candidate is a prerelease")
            publish = release["draft"]
        else:
            # Ordinary commits after a release retain its version; they are not candidates.
            print(f"{tag} belongs to a different SHA; no stable release for this commit")
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"tag={tag}\npublish={str(publish).lower()}\n")


def archive_name(tag, target):
    suffix = ".zip" if "windows" in target else ".tar.gz"
    return f"bend2-lsp-{tag}-{target}{suffix}"


def package():
    version = verify()
    target = os.environ["RELEASE_TARGET"]
    if target not in TARGETS:
        raise ValueError("unsupported native release target")
    tag = os.environ["RELEASE_TAG"]
    executable = "bend2-lsp.exe" if "windows" in target else "bend2-lsp"
    binary = Path("target/release") / executable
    root = f"bend2-lsp-{tag}-{target}"
    metadata = {
        "source_sha": os.environ["RELEASE_SHA"],
        "tag": tag,
        "version": version,
        "target": target,
        "channel": os.environ["RELEASE_CHANNEL"],
    }
    files = {
        executable: binary.read_bytes(),
        "LICENSE": Path("LICENSE").read_bytes(),
        "README.md": Path("README.md").read_bytes(),
        "RELEASE-METADATA.json": (json.dumps(metadata, indent=2) + "\n").encode(),
    }
    dist = Path("dist")
    dist.mkdir(exist_ok=True)
    archive = dist / archive_name(tag, target)
    if "windows" in target:
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for name, content in files.items():
                member = zipfile.ZipInfo(f"{root}/{name}", date_time=(1980, 1, 1, 0, 0, 0))
                member.compress_type = zipfile.ZIP_DEFLATED
                member.external_attr = (0o755 if name == executable else 0o644) << 16
                output.writestr(member, content)
        with zipfile.ZipFile(archive) as packaged:
            tested_bytes = packaged.read(f"{root}/{executable}")
    else:
        with archive.open("wb") as raw:
            with gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as output:
                    for name, content in files.items():
                        member = tarfile.TarInfo(f"{root}/{name}")
                        member.size = len(content)
                        member.mode = 0o755 if name == executable else 0o644
                        output.addfile(member, io.BytesIO(content))
        with tarfile.open(archive, "r:gz") as packaged:
            member = packaged.extractfile(f"{root}/{executable}")
            if member is None:
                raise ValueError("archive has no executable")
            tested_bytes = member.read()
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    Path(f"{archive}.sha256").write_text(f"{checksum}  {archive.name}\n")
    # Exercise the bytes read back from the actual archive, not an unrelated cargo binary.
    smoke = Path(os.environ["RUNNER_TEMP"]) / "release-e2e" / executable
    smoke.parent.mkdir(parents=True, exist_ok=True)
    smoke.write_bytes(tested_bytes)
    smoke.chmod(0o755)
    with Path(os.environ["GITHUB_ENV"]).open("a") as output:
        output.write(f"BEND2_LSP_TEST_BINARY={smoke.resolve()}\n")
    print(f"Packaged {archive}; E2E executable: {smoke}")


def validate_artifacts(tag, version):
    dist = Path("dist")
    expected = set()
    checksums = []
    for target in TARGETS:
        name = archive_name(tag, target)
        expected.update((name, f"{name}.sha256"))
        archive = dist / name
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        checksum = f"{digest}  {name}\n"
        if (dist / f"{name}.sha256").read_text() != checksum:
            raise ValueError(f"archive checksum mismatch: {name}")
        checksums.append(checksum)
        metadata_path = f"bend2-lsp-{tag}-{target}/RELEASE-METADATA.json"
        if name.endswith(".zip"):
            with zipfile.ZipFile(archive) as packaged:
                metadata = json.loads(packaged.read(metadata_path))
        else:
            with tarfile.open(archive, "r:gz") as packaged:
                member = packaged.extractfile(metadata_path)
                if member is None:
                    raise ValueError(f"missing release metadata: {name}")
                metadata = json.load(member)
        if metadata != {
            "source_sha": os.environ["RELEASE_SHA"],
            "tag": tag,
            "version": version,
            "target": target,
            "channel": os.environ["RELEASE_CHANNEL"],
        }:
            raise ValueError(f"archive identity mismatch: {name}")
    actual = {entry.name for entry in dist.iterdir()}
    if actual != expected:
        raise ValueError("release artifacts are incomplete or contain unexpected files")
    (dist / "SHA256SUMS").write_text("".join(checksums))
    return sorted(expected | {"SHA256SUMS"})


def publish():
    version = verify()
    tag = os.environ["RELEASE_TAG"]
    sha = os.environ["RELEASE_SHA"]
    nightly = os.environ["RELEASE_CHANNEL"] == "nightly"
    assets = validate_artifacts(tag, version)
    actual_sha = tag_sha(tag, missing_ok=True)
    if actual_sha is not None and actual_sha != sha:
        raise ValueError("existing release tag references a different source SHA")
    release = release_for_tag(tag)
    if not nightly and release is None:
        raise ValueError("stable publication requires a release-please draft")
    if release is not None:
        if actual_sha != sha or release["prerelease"] != nightly:
            raise ValueError("existing release identity or channel mismatch")
        if not release["draft"]:
            if {asset["name"] for asset in release["assets"]} != set(assets):
                raise ValueError("published release is incomplete; refusing to mutate it")
            print(f"{tag} is already public at {sha}; leaving all assets unchanged")
            return
    else:
        if actual_sha is None:
            api("git/refs", "POST", {"ref": f"refs/tags/{tag}", "sha": sha})
        release = api("releases", "POST", {
            "tag_name": tag,
            "target_commitish": sha,
            "name": tag,
            "body": f"Native nightly binaries from `{sha}`.\n\n"
                    "All six targets passed the full release suite and packaged-binary E2E. "
                    "See docs/releases.md for platform and installation limits.",
            "draft": True,
            "prerelease": True,
            "make_latest": "false",
        })
    if tag_sha(tag) != sha:
        raise ValueError("release tag changed before artifact upload")
    subprocess.run([
        "gh", "release", "upload", tag,
        *[str(Path("dist") / name) for name in assets],
        "--clobber", "--repo", os.environ["GITHUB_REPOSITORY"],
    ], check=True)
    uploaded = api(f"releases/{release['id']}")
    if {asset["name"] for asset in uploaded["assets"]} != set(assets):
        raise ValueError("uploaded asset set does not match all six tested targets")
    if tag_sha(tag) != sha:
        raise ValueError("release tag changed before publication")
    if nightly and api("branches/main")["commit"]["sha"] != sha:
        print(f"Skipping obsolete nightly {tag}: main advanced after this quality run")
        return
    api(f"releases/{release['id']}", "PATCH", {
        "draft": False,
        "prerelease": nightly,
        "make_latest": "false" if nightly else "true",
    })
    print(f"Published {tag} at {sha} with six tested native archives and SHA256SUMS")


if __name__ == "__main__":
    commands = {"verify": verify, "candidate": candidate, "package": package, "publish": publish}
    if len(sys.argv) != 2 or sys.argv[1] not in commands:
        raise SystemExit("usage: release.py {verify|candidate|package|publish}")
    commands[sys.argv[1]]()
