#!/usr/bin/env python3
"""Install the checksum-pinned official Neovim for Linux x86_64 quality E2E."""

import argparse
import hashlib
import io
from pathlib import Path, PurePosixPath
import platform
import shutil
import tarfile
import urllib.request


VERSION = "0.12.5"
ASSET = "nvim-linux-x86_64.tar.gz"
# Official v0.12.5 release asset 526503737; also verified against downloaded bytes.
# https://github.com/neovim/neovim/releases/tag/v0.12.5
SHA256 = "bce0f56eda1f1b1db6eee8f4133d7a38813ea07933837dd1777411ca384c6875"
ARCHIVE_SIZE = 11401951
ROOT = "nvim-linux-x86_64"


def install(destination: Path) -> None:
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        raise ValueError("bootstrap supports Linux x86_64 only; supply --nvim to the E2E launcher elsewhere")
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"installation destination must not exist: {destination}")
    url = f"https://github.com/neovim/neovim/releases/download/v{VERSION}/{ASSET}"
    with urllib.request.urlopen(url, timeout=60) as response:
        data = response.read(ARCHIVE_SIZE + 1)
    if len(data) != ARCHIVE_SIZE or hashlib.sha256(data).hexdigest() != SHA256:
        raise ValueError("Neovim archive size/checksum mismatch; refusing extraction")
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
                raise ValueError(f"unsafe Neovim archive member: {member.name}")
            seen.add(str(path))
            total += member.size
        if total > 128 * 1024 * 1024:
            raise ValueError("unexpected Neovim unpacked size")
        by_name = {str(PurePosixPath(member.name)): member for member in members}
        for required in (f"{ROOT}/bin/nvim", f"{ROOT}/share/nvim/runtime/lua/vim/lsp/handlers.lua"):
            if required not in by_name or not by_name[required].isfile():
                raise ValueError(f"missing Neovim archive member: {required}")
        if not by_name[f"{ROOT}/bin/nvim"].mode & 0o111:
            raise ValueError("Neovim archive executable metadata is invalid")
        # Validate all parent metadata before creating anything. Never extract
        # links, special files, ownership metadata, or unvalidated tar paths.
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
                        raise ValueError(f"unreadable Neovim archive member: {member.name}")
                    with source, output.open("xb") as target:
                        shutil.copyfileobj(source, target)
                    output.chmod(0o755 if member.mode & 0o111 else 0o644)
        except BaseException:
            shutil.rmtree(destination)
            raise
    print(f"Installed official Neovim {VERSION} ({SHA256}): {destination / 'bin/nvim'}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", type=Path, required=True)
    arguments = parser.parse_args()
    install(arguments.destination.absolute())


if __name__ == "__main__":
    main()
