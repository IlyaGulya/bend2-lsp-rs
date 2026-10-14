#!/usr/bin/env bash
# Install only the reviewed upstream source into a dedicated native-tool root.
set -euo pipefail

version=3.27.1
source_url="https://sourceware.org/pub/valgrind/valgrind-$version.tar.bz2"
source_sha512=4522e345fe31bc10478f21ab261cf7b3e509d80fe98fd0c7c9778ac9a9f90e1d8fa6e54a37c2e23114c01c95131035103fc1e78661710d7f45f5d564a31c1015

fail() {
  printf 'Valgrind installation failed: %s\n' "$*" >&2
  exit 1
}

[[ "$(uname -s)" == Linux ]] || fail 'only native 64-bit Linux hosts are supported'
host_arch="$(uname -m)"
case "$host_arch" in
  x86_64) platform=amd64-linux ;;
  aarch64) platform=arm64-linux ;;
  *) fail "unsupported Linux architecture: $host_arch" ;;
esac
[[ "$(getconf LONG_BIT)" == 64 ]] || fail 'a 64-bit Linux userspace is required'
[[ "${VALGRIND_INSTALL_ROOT:-}" == /* ]] || fail 'VALGRIND_INSTALL_ROOT must be an absolute dedicated installation path'
[[ ! -L "$VALGRIND_INSTALL_ROOT" ]] || fail 'VALGRIND_INSTALL_ROOT must not be a symbolic link'
root="$(realpath -m -- "$VALGRIND_INSTALL_ROOT")"
[[ "$root" != / ]] || fail 'VALGRIND_INSTALL_ROOT must not be the filesystem root'

# The same source identity and runtime-byte validation applies to staged builds
# and restored caches. Paths in provenance are relative so no build/temp paths
# leak into the immutable installed identity.
installation_identity() {
  python3 - "$1" "$2" "$version" "$source_url" "$source_sha512" "$host_arch" "$platform" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

root = Path(sys.argv[1])
mode, version, source_url, source_sha512, host_arch, platform = sys.argv[2:]
identity = {
    "format_version": 1,
    "version": version,
    "source_url": source_url,
    "source_sha512": source_sha512,
    "host_arch": host_arch,
}

try:
    if not root.is_dir() or root.is_symlink():
        raise ValueError("installation root is not a regular directory")
    resolved_root = root.resolve(strict=True)
    runtime_files = []
    for directory in (root / "bin", root / "libexec" / "valgrind"):
        if not directory.is_dir() or directory.is_symlink():
            raise ValueError(f"missing or linked runtime directory: {directory}")
        for path in sorted(directory.rglob("*")):
            if path.is_symlink():
                raise ValueError(f"unexpected linked runtime path: {path}")
            if path.is_dir():
                continue
            if not path.is_file() or resolved_root not in path.resolve(strict=True).parents:
                raise ValueError(f"invalid runtime file: {path}")
            digest = hashlib.sha256()
            with path.open("rb") as installed:
                for block in iter(lambda: installed.read(1024 * 1024), b""):
                    digest.update(block)
            runtime_files.append({
                "path": path.relative_to(root).as_posix(),
                "sha256": digest.hexdigest(),
            })
    native_files = (
        "bin/valgrind",
        f"libexec/valgrind/callgrind-{platform}",
        f"libexec/valgrind/memcheck-{platform}",
    )
    required_files = (*native_files, "bin/callgrind_annotate")
    for relative in required_files:
        path = root / relative
        if not path.is_file() or not os.access(path, os.X_OK):
            raise ValueError(f"missing executable: {relative}")
    expected_machine = {"x86_64": 62, "aarch64": 183}[host_arch]
    for relative in native_files:
        with (root / relative).open("rb") as installed:
            header = installed.read(20)
        if (
            header[:6] != b"\x7fELF\x02\x01"
            or int.from_bytes(header[18:20], "little") != expected_machine
        ):
            raise ValueError(f"not a native 64-bit Linux ELF: {relative}")
    identity["installed_files"] = runtime_files
    provenance = root / "provenance.json"
    if mode == "validate":
        if provenance.is_symlink():
            raise ValueError("provenance must not be a symbolic link")
        recorded = json.loads(provenance.read_text(encoding="utf-8"))
        if recorded != identity:
            raise ValueError("source/version/host/runtime hashes do not match provenance")
    elif mode != "record":
        raise ValueError(f"unsupported provenance operation: {mode}")

    environment = os.environ.copy()
    # Use the installed engines, never an ambient apt installation or options.
    environment["VALGRIND_LIB"] = str(root / "libexec" / "valgrind")
    environment.pop("VALGRIND_OPTS", None)
    observed = subprocess.run(
        [str(root / "bin" / "valgrind"), "--version"],
        check=True, capture_output=True, text=True, env=environment, timeout=30,
    ).stdout.strip()
    if observed != f"valgrind-{version}":
        raise ValueError(f"unexpected installed version: {observed!r}")
    if mode == "record":
        with provenance.open("x", encoding="utf-8") as output:
            json.dump(identity, output, indent=2, sort_keys=True)
            output.write("\n")
        provenance.chmod(0o444)
except (OSError, ValueError, subprocess.SubprocessError) as error:
    print(f"Valgrind installation identity rejected: {error}", file=sys.stderr)
    sys.exit(1)
PY
}

if [[ -e "$root" ]]; then
  installation_identity "$root" validate || fail "cached installation at $root is invalid; remove this dedicated root and retry to rebuild from verified source"
  printf 'Reusing verified valgrind-%s at %s\n' "$version" "$root"
  exit 0
fi

for prerequisite in curl sha512sum tar make gcc perl nproc; do
  command -v "$prerequisite" >/dev/null || fail "missing source-build prerequisite: $prerequisite"
done

# Stage on the destination filesystem, so promotion cannot leave a partial
# copied installation. Never delete or overwrite an existing installation root.
parent="$(dirname "$root")"
mkdir -p "$parent"
temporary="$(mktemp -d "$parent/.valgrind-build.XXXXXXXX")"
trap 'rm -rf -- "$temporary"' EXIT
archive="$temporary/valgrind-$version.tar.bz2"
curl --fail --show-error --silent --location "$source_url" --output "$archive"
printf '%s  %s\n' "$source_sha512" "$archive" | sha512sum --check -
tar -xjf "$archive" -C "$temporary"
mkdir "$temporary/build"
(
  cd "$temporary/build"
  "$temporary/valgrind-$version/configure" \
    --prefix="$root" --libexecdir="$root/libexec" --enable-only64bit
)
make -C "$temporary/build" -j"$(nproc)"
make -C "$temporary/build" DESTDIR="$temporary/install" install
staged_root="$temporary/install$root"
installation_identity "$staged_root" record
[[ ! -e "$root" && ! -L "$root" ]] || fail "installation root appeared during build: $root"
mv --no-target-directory --no-clobber -- "$staged_root" "$root"
[[ ! -e "$staged_root" ]] || fail "refusing to overwrite installation root: $root"
printf 'Installed verified valgrind-%s at %s\n' "$version" "$root"
printf 'Add %s/bin to PATH for both baseline and candidate measurements.\n' "$root"
