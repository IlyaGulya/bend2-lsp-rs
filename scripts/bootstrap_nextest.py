#!/usr/bin/env python3
"""Install the checksum-pinned official native cargo-nextest without a source build."""

import argparse
import hashlib
import io
from pathlib import Path
import shutil
import tarfile
import urllib.request


VERSION = "0.9.131"
# Official immutable release assets; sizes and SHA-256 digests were independently
# verified against downloaded archive bytes (not only the release API metadata).
# https://github.com/nextest-rs/nextest/releases/tag/cargo-nextest-0.9.131
# Each reviewed archive contains exactly one regular executable at its root.
ARCHIVES = {
    "x86_64-unknown-linux-gnu": (
        11645538,
        "8e38b16299864c9f597c9a1e2caf25b7e8b598ffc659ec014c2c735a9befd8fa",
        33265192,
    ),  # asset 376664322
    "aarch64-unknown-linux-gnu": (
        8492217,
        "ce3bc715344d51e6f0fbb944489e37d17e25d24dfafaf861cc37643cfe7de663",
        21272496,
    ),  # asset 376668748
    "universal-apple-darwin": (
        16020170,
        "1cd1241477adb035a57fe230eb20d37f1bbaa0296269803f44a575b4c5a919ab",
        38525456,
    ),  # asset 376665907; native x86_64 and aarch64 slices
    "x86_64-pc-windows-msvc": (
        7453377,
        "687b66200af37d1bff0a071ed2c5f3cdac29f68018ac7a77108fb5461f544ab1",
        18737968,
    ),  # asset 376670013
    "aarch64-pc-windows-msvc": (
        7107349,
        "ec88e8e75e395849383565a61052be7621ff164d145013a1277172e6343bacd2",
        16367408,
    ),  # asset 376670943
}

# The workflow verifies runner.arch and the native Rust host before invoking us.
# Choose that host explicitly: Python itself can be emulated on Windows ARM64.
NATIVE_TARGETS = {
    "x86_64-unknown-linux-gnu": "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu": "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin": "universal-apple-darwin",
    "aarch64-apple-darwin": "universal-apple-darwin",
    "x86_64-pc-windows-msvc": "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc": "aarch64-pc-windows-msvc",
}


def install(destination: Path, native_target: str) -> None:
    target = NATIVE_TARGETS[native_target]
    archive_size, checksum, executable_size = ARCHIVES[target]
    executable = "cargo-nextest.exe" if "windows" in target else "cargo-nextest"
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"installation destination must not exist: {destination}")
    asset = f"cargo-nextest-{VERSION}-{target}.tar.gz"
    url = f"https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-{VERSION}/{asset}"
    with urllib.request.urlopen(url, timeout=60) as response:
        data = response.read(archive_size + 1)
    if len(data) != archive_size or hashlib.sha256(data).hexdigest() != checksum:
        raise ValueError("nextest archive size/checksum mismatch; refusing extraction")
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        members = archive.getmembers()
        if len(members) != 1:
            raise ValueError("unexpected nextest archive contents")
        member = members[0]
        if (
            member.name != executable or not member.isfile()
            or member.size != executable_size or member.mode != 0o755
        ):
            raise ValueError(f"unsafe nextest archive member: {member.name}")
        source = archive.extractfile(member)
        if source is None:
            raise ValueError(f"unreadable nextest archive member: {member.name}")
        # Copy only the validated root executable, never tar paths, links,
        # ownership, or permission metadata. Cargo discovers it through PATH.
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.mkdir()
        try:
            output = destination / executable
            with source, output.open("xb") as installed:
                shutil.copyfileobj(source, installed)
            output.chmod(0o755)
        except BaseException:
            shutil.rmtree(destination)
            raise
    print(f"Installed official cargo-nextest {VERSION} ({target}, {checksum}): {output}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True)
    parser.add_argument("--target", choices=NATIVE_TARGETS, required=True)
    arguments = parser.parse_args()
    install(arguments.destination.absolute(), arguments.target)


if __name__ == "__main__":
    main()
