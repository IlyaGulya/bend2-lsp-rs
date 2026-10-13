#!/usr/bin/env python3
"""Install genuine checksum-pinned Bend 2.0.36 for macOS ARM64 native E2E."""

import argparse
import hashlib
import io
from pathlib import Path, PurePosixPath
import platform
import shutil
import tarfile
import urllib.request


VERSION = "2.0.36"
# Official release asset 617610925, published from ae1101ca7d15364f9274fa6b1367d175884a7da7.
# https://github.com/bendlang/bend/releases/tag/v2.0.36
# The asset-ID URL avoids mutable tag/name resolution. The release itself is not
# marked immutable upstream; the reviewed SHA-256 is the immutable byte identity.
URL = "https://api.github.com/repos/bendlang/bend/releases/assets/617610925"
SHA256 = "2876687ceb0aba836abc98b3f62c8fc9fb612d124ae2af6814d5ec937760878d"
ARCHIVE_SIZE = 23802450
ROOT = "bend"
# Independently checked against the downloaded distribution and installed bundle.
# The archive digest also binds every effect resource and guide, not just these
# required entry points. Keep bin/ adjacent to bend2/ for Bend's resource lookup.
REQUIRED_FILES = {
    "bin/bend": (63423712, "4d7280f366661d9b63b418eb70ca54ea1a867a23b196cc4b5f1f75e049cb80d1"),
    "bend2/base.bend": (75064, "99ac43f2b2bb3e3f39acdcedcecbbd3cb44749ce13969c827d6973fa66f7facf"),
    "bend2/bendtt.lean": (171316, "c40d2b219c77c64de1fd6c32224cc53fa928436d8ab7df5bf5a7e585d556cc32"),
}


def install(destination: Path) -> None:
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise ValueError("Bend native E2E bootstrap supports macOS ARM64 only")
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"installation destination must not exist: {destination}")
    request = urllib.request.Request(URL, headers={"Accept": "application/octet-stream"})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(ARCHIVE_SIZE + 1)
    if len(data) != ARCHIVE_SIZE or hashlib.sha256(data).hexdigest() != SHA256:
        raise ValueError("Bend archive size/checksum mismatch; refusing extraction")
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        members = archive.getmembers()
        seen = set()
        total = 0
        for member in members:
            path = PurePosixPath(member.name)
            if (
                path.is_absolute() or ".." in path.parts or "\\" in member.name
                or not path.parts or path.parts[0] != ROOT
                or str(path) in seen or not (member.isdir() or member.isfile())
                or member.size < 0 or member.mode & 0o7000
            ):
                raise ValueError(f"unsafe Bend archive member: {member.name}")
            seen.add(str(path))
            total += member.size
        if total > 128 * 1024 * 1024:
            raise ValueError("unexpected Bend unpacked size")
        by_name = {str(PurePosixPath(member.name)): member for member in members}
        for relative, (size, checksum) in REQUIRED_FILES.items():
            required = f"{ROOT}/{relative}"
            member = by_name.get(required)
            if member is None or not member.isfile() or member.size != size:
                raise ValueError(f"missing or invalid Bend archive member: {required}")
            source = archive.extractfile(member)
            if source is None:
                raise ValueError(f"unreadable Bend archive member: {required}")
            with source:
                if hashlib.file_digest(source, "sha256").hexdigest() != checksum:
                    raise ValueError(f"Bend archive member checksum mismatch: {required}")
        if by_name[f"{ROOT}/bin/bend"].mode != 0o755:
            raise ValueError("Bend executable metadata is invalid")
        effects = by_name.get(f"{ROOT}/bend2/effs")
        if effects is None or not effects.isdir():
            raise ValueError("missing Bend effect resources")
        # Follow the existing bootstrap convention: validate all parents before
        # creating files; never extract links, ownership or tar permission bits.
        for member in members:
            path = PurePosixPath(member.name)
            for parent in path.parents:
                if str(parent) in by_name and not by_name[str(parent)].isdir():
                    raise ValueError(f"non-directory archive parent: {parent}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.mkdir()
        try:
            for member in members:
                relative = PurePosixPath(member.name).relative_to(ROOT)
                output = destination.joinpath(*relative.parts)
                if member.isdir():
                    output.mkdir(parents=True, exist_ok=True)
                else:
                    output.parent.mkdir(parents=True, exist_ok=True)
                    source = archive.extractfile(member)
                    if source is None:
                        raise ValueError(f"unreadable Bend archive member: {member.name}")
                    with source, output.open("xb") as target:
                        shutil.copyfileobj(source, target)
                    output.chmod(0o755 if relative == PurePosixPath("bin/bend") else 0o644)
        except BaseException:
            shutil.rmtree(destination)
            raise
    print(f"Installed official Bend {VERSION} ({SHA256}): {destination / 'bin/bend'}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True)
    arguments = parser.parse_args()
    install(arguments.destination.absolute())


if __name__ == "__main__":
    main()
