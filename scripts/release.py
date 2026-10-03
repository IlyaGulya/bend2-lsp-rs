#!/usr/bin/env python3
"""Package native binaries and publish only complete, SHA-verified releases."""

import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
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


def asset_name(target):
    suffix = ".exe" if "windows" in target else ""
    return f"bend2-lsp-{target}{suffix}"


def archive_name(target):
    suffix = ".zip" if "windows" in target else ".tar.gz"
    return f"bend2-lsp-{target}{suffix}"


def write_archive(asset, target, archive):
    root = f"bend2-lsp-{target}"
    executable = "bend2-lsp.exe" if "windows" in target else "bend2-lsp"
    # Fixed timestamps, ownership and modes make retries reproduce identical bytes.
    if "windows" in target:
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            member = zipfile.ZipInfo(executable)
            member.create_system = 3
            member.external_attr = 0o100755 << 16
            member.compress_type = zipfile.ZIP_DEFLATED
            with asset.open("rb") as source, output.open(member, "w") as destination:
                shutil.copyfileobj(source, destination)
    else:
        with archive.open("wb") as destination:
            with gzip.GzipFile(filename="", mode="wb", fileobj=destination, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as output:
                    directory = tarfile.TarInfo(root)
                    directory.type = tarfile.DIRTYPE
                    directory.mode = 0o755
                    output.addfile(directory)
                    member = tarfile.TarInfo(f"{root}/{executable}")
                    member.mode = 0o755
                    member.size = asset.stat().st_size
                    with asset.open("rb") as source:
                        output.addfile(member, source)


def validate_archive(archive, target, asset, digest):
    root = f"bend2-lsp-{target}"
    executable = "bend2-lsp.exe" if "windows" in target else "bend2-lsp"
    member_name = f"{root}/{executable}"
    size = asset.stat().st_size
    try:
        if "windows" in target:
            with zipfile.ZipFile(archive) as source:
                members = source.infolist()
                if [member.filename for member in members] != [executable]:
                    raise ValueError(f"archive member layout mismatch: {archive.name}")
                member = members[0]
                if (
                    member.is_dir() or member.create_system != 3
                    or member.external_attr >> 16 != 0o100755 or member.file_size != size
                    or any(entry.flag_bits & 1 for entry in members)
                ):
                    raise ValueError(f"unsafe archive member: {archive.name}")
                with source.open(member) as binary:
                    extracted_digest = hashlib.file_digest(binary, "sha256").hexdigest()
        else:
            with tarfile.open(archive, "r:gz") as source:
                members = source.getmembers()
                if [member.name for member in members] != [root, member_name]:
                    raise ValueError(f"archive member layout mismatch: {archive.name}")
                directory, member = members
                if (
                    directory.type != tarfile.DIRTYPE or directory.size != 0
                    or directory.mode != 0o755 or member.type != tarfile.REGTYPE
                    or member.mode != 0o755 or member.size != size
                    or any(entry.linkname or entry.pax_headers for entry in members)
                ):
                    raise ValueError(f"unsafe archive member: {archive.name}")
                with source.extractfile(member) as binary:
                    extracted_digest = hashlib.file_digest(binary, "sha256").hexdigest()
    except (tarfile.TarError, zipfile.BadZipFile, OSError, EOFError) as error:
        raise ValueError(f"invalid native archive: {archive.name}") from error
    if extracted_digest != digest:
        raise ValueError(f"archive executable bytes mismatch: {archive.name}")


def file_sha256(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def package():
    version = verify()
    target = os.environ["RELEASE_TARGET"]
    if target not in TARGETS:
        raise ValueError("unsupported native release target")
    tag = os.environ["RELEASE_TAG"]
    executable = "bend2-lsp.exe" if "windows" in target else "bend2-lsp"
    binary = Path("target/release") / executable
    dist = Path("dist")
    dist.mkdir(exist_ok=True)
    asset = dist / asset_name(target)
    archive = dist / archive_name(target)
    for path in (
        asset, Path(f"{asset}.sha256"), Path(f"{asset}.metadata.json"),
        archive, Path(f"{archive}.sha256"),
    ):
        if path.is_symlink() or (path.exists() and not path.is_file()):
            raise ValueError(f"release artifact must be a regular file: {path.name}")
    shutil.copyfile(binary, asset)
    asset.chmod(0o755)
    checksum = file_sha256(asset)
    Path(f"{asset}.sha256").write_text(f"{checksum}  {asset.name}\n", encoding="utf-8", newline="\n")
    write_archive(asset, target, archive)
    archive_checksum = file_sha256(archive)
    Path(f"{archive}.sha256").write_text(
        f"{archive_checksum}  {archive.name}\n", encoding="utf-8", newline="\n",
    )
    # CI-only metadata binds the source identity to the exact tested/uploaded bytes.
    metadata = {
        "source_sha": os.environ["RELEASE_SHA"],
        "tag": tag,
        "version": version,
        "target": target,
        "channel": os.environ["RELEASE_CHANNEL"],
        "asset": asset.name,
        "sha256": checksum,
        "archive": archive.name,
        "archive_sha256": archive_checksum,
    }
    Path(f"{asset}.metadata.json").write_text(
        json.dumps(metadata, indent=2) + "\n", encoding="utf-8", newline="\n",
    )
    with Path(os.environ["GITHUB_ENV"]).open("a", encoding="utf-8", newline="\n") as output:
        output.write(f"BEND2_LSP_TEST_BINARY={asset.resolve()}\n")
    print(f"Packaged {asset}; E2E executable: {asset.resolve()}")


def validate_artifacts(tag, version, *, include_installers=True):
    dist = Path("dist")
    expected = set()
    public = set()
    checksums = []
    for target in TARGETS:
        name = asset_name(target)
        archive = dist / archive_name(target)
        public.update((name, f"{name}.sha256", archive.name, f"{archive.name}.sha256"))
        expected.update((
            name, f"{name}.sha256", f"{name}.metadata.json",
            archive.name, f"{archive.name}.sha256",
        ))
        asset = dist / name
        for path in (
            asset, dist / f"{name}.sha256", dist / f"{name}.metadata.json",
            archive, dist / f"{archive.name}.sha256",
        ):
            if path.is_symlink() or not path.is_file():
                raise ValueError(f"release artifact must be a regular file: {path.name}")
        digest = file_sha256(asset)
        checksum = f"{digest}  {name}\n"
        if (dist / f"{name}.sha256").read_text() != checksum:
            raise ValueError(f"executable checksum mismatch: {name}")
        checksums.append(checksum)
        archive_digest = file_sha256(archive)
        archive_checksum = f"{archive_digest}  {archive.name}\n"
        if (dist / f"{archive.name}.sha256").read_text() != archive_checksum:
            raise ValueError(f"archive checksum mismatch: {archive.name}")
        checksums.append(archive_checksum)
        metadata = json.loads((dist / f"{name}.metadata.json").read_text())
        if metadata != {
            "source_sha": os.environ["RELEASE_SHA"],
            "tag": tag,
            "version": version,
            "target": target,
            "channel": os.environ["RELEASE_CHANNEL"],
            "asset": name,
            "sha256": digest,
            "archive": archive.name,
            "archive_sha256": archive_digest,
        }:
            raise ValueError(f"executable identity mismatch: {name}")
        validate_archive(archive, target, asset, digest)
    if include_installers:
        from dist_installers import validate_installers

        installer_expected, installer_public, installer_checksums = validate_installers(tag, version)
        expected.update(installer_expected)
        public.update(installer_public)
        checksums.extend(installer_checksums)
    actual = {entry.name for entry in dist.iterdir()}
    # Revalidating the same complete directory is safe; never trust its old manifest.
    if actual not in (expected, expected | {"SHA256SUMS"}):
        raise ValueError("release artifacts are incomplete or contain unexpected files")
    manifest = dist / "SHA256SUMS"
    if manifest.is_symlink() or (manifest.exists() and not manifest.is_file()):
        raise ValueError("checksum manifest must be a regular file")
    manifest.write_text("".join(checksums), encoding="utf-8", newline="\n")
    return sorted(public | {"SHA256SUMS"})


def validate_uploaded_assets(release, assets, allow_missing=False):
    uploaded = {asset["name"]: asset for asset in release["assets"]}
    expected = set(assets)
    if len(uploaded) != len(release["assets"]) or not set(uploaded) <= expected:
        raise ValueError("uploaded asset set contains unexpected or duplicate assets")
    missing = expected - set(uploaded)
    if missing and not allow_missing:
        raise ValueError("uploaded asset set does not match all six tested targets")
    for name, asset in uploaded.items():
        path = Path("dist") / name
        if asset.get("digest") != f"sha256:{file_sha256(path)}" or asset["size"] != path.stat().st_size:
            raise ValueError(f"uploaded asset bytes do not match tested artifact: {name}")
    return sorted(missing)


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
            validate_uploaded_assets(release, assets)
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
                    "All six targets passed the full release suite and direct-executable E2E. "
                    "See docs/releases.md for platform and installation limits.",
            "draft": True,
            "prerelease": True,
            "make_latest": "false",
        })
    if tag_sha(tag) != sha:
        raise ValueError("release tag changed before artifact upload")
    # A retry may resume a partial draft, but must never replace existing assets.
    release = api(f"releases/{release['id']}")
    if release["prerelease"] != nightly:
        raise ValueError("existing release channel changed before artifact upload")
    missing = validate_uploaded_assets(release, assets, allow_missing=release["draft"])
    if not release["draft"]:
        print(f"{tag} became public at {sha}; leaving all assets unchanged")
        return
    if missing:
        subprocess.run([
            "gh", "release", "upload", tag,
            *[str(Path("dist") / name) for name in missing],
            "--repo", os.environ["GITHUB_REPOSITORY"],
        ], check=True)
    uploaded = api(f"releases/{release['id']}")
    validate_uploaded_assets(uploaded, assets)
    if uploaded["prerelease"] != nightly:
        raise ValueError("existing release channel changed before publication")
    if not uploaded["draft"]:
        print(f"{tag} became public at {sha}; leaving all assets unchanged")
        return
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
    print(f"Published {tag} at {sha} with six tested native executables and SHA256SUMS")


if __name__ == "__main__":
    commands = {"verify": verify, "candidate": candidate, "package": package, "publish": publish}
    if len(sys.argv) != 2 or sys.argv[1] not in commands:
        raise SystemExit("usage: release.py {verify|candidate|package|publish}")
    commands[sys.argv[1]]()
