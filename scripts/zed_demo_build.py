#!/usr/bin/env python3
"""Build the headless bend2-lint capture driver from an isolated Zed clone.

The capture driver is a temporary ``[[bin]]`` target inside a real Zed
workspace: it boots a genuine windowless ``Workspace`` on Zed's headless Metal
renderer and dispatches real GPUI input events. The three proven driver sources
live in :data:`SOURCE_ASSETS_DIR` and are the only editor code this tool adds.

``build_driver`` never touches the Zed checkout it is given. It validates that
checkout's revision and windowless API, clones it once into a caller-owned
cache, copies the bundled driver sources into the clone, wires exactly one
``[[bin]]`` and two workspace dependencies into the clone's
``crates/zed/Cargo.toml``, and builds ``zed --bin bend2_lint_demo --features
visual-tests`` with the pinned ``1.98.1`` toolchain. A matching cache is reused
without recloning or rebuilding. Changed bundled driver sources at the same
source revision are treated as normal iteration: only those owned files are
refreshed and cargo rebuilds into the existing target. A foreign or conflicting
cache is rejected instead of repaired.

The tested Zed source pin is :data:`TESTED_ZED_REVISION` (Zed 1.22). Other
checkouts are accepted only while they still expose the genuine windowless API
listed in ``_HEADLESS_API_MARKERS``. The resulting program is macOS-only (the
headless renderer is Metal). This module never installs toolchains or downloads
drivers: it requires ``rustup`` and the pinned ``1.98.1`` toolchain to be
installed already, disables rustup auto-install, and fails with an explicit
command to run instead.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path

from zed_demo_process import owned_process

TESTED_ZED_REVISION = "76659a55a8c10ed355a070f8764a0b1733e3c115"
TESTED_ZED_VERSION = "1.22.0"
RUST_TOOLCHAIN = "1.98.1"
CACHE_SCHEMA = "bend2-zed-demo/build-cache/1"
MARKER_FILENAME = "marker.json"
DRIVER_BIN_NAME = "bend2_lint_demo"
DRIVER_CRATE = "zed"
DRIVER_FEATURES = "visual-tests"
DRIVER_MANIFEST_PATH = "src/bend2_lint_demo.rs"

ASSETS_DIR = Path(__file__).resolve().parent / "zed_demo_assets"
SOURCE_ASSETS_DIR = ASSETS_DIR / "zed-source"
FIXTURES_DIR = ASSETS_DIR / "fixtures"
FIXTURE_SCENES = (
    "clip1-unsaved-entry",
    "clip2-imported-type",
    "clip3-dangerous-action",
)

_SOURCE_ASSET_PREFIX = "crates/zed/src/"
REQUIRED_SOURCE_ASSETS = (
    "crates/zed/src/bend2_lint_demo.rs",
    "crates/zed/src/bend2_lint_demo_scenes.rs",
    "crates/zed/src/bend2_lint_demo_interactions.rs",
)

# The genuine windowless surface the driver compiles against. Every entry must
# exist with the listed markers, otherwise the checkout cannot host the driver.
_HEADLESS_API_MARKERS = (
    (
        "crates/gpui/src/app/headless_app_context.rs",
        ("pub struct HeadlessAppContext", "pub fn capture_screenshot"),
    ),
    (
        "crates/gpui_platform/src/gpui_platform.rs",
        ("pub fn current_headless_renderer", "MetalHeadlessRenderer::new()"),
    ),
    (
        "crates/gpui_apple/src/metal_renderer.rs",
        (
            "pub struct MetalHeadlessRenderer",
            "impl gpui::PlatformHeadlessRenderer for MetalHeadlessRenderer",
        ),
    ),
)

_WORKSPACE_DEPENDENCIES = ("async-trait", "lsp")
_DEPENDENCY_KEY = re.compile(r"^([A-Za-z0-9_-]+)\s*(?:\.|=)")
_REVISION = re.compile(r"^[0-9a-f]{40}$")

_DRIVER_BIN_LINES = (
    "[[bin]]\n",
    f'name = "{DRIVER_BIN_NAME}"\n',
    f'path = "{DRIVER_MANIFEST_PATH}"\n',
    f'required-features = ["{DRIVER_FEATURES}"]\n',
    "\n",
)


class BuildDriverError(RuntimeError):
    """A source, cache or manifest problem the caller must resolve explicitly."""


@dataclass(frozen=True)
class BuiltDriver:
    """A built capture driver and the source tree it was compiled from."""

    binary: Path
    source: Path
    revision: str


@dataclass(frozen=True)
class _CachePaths:
    root: Path
    source: Path
    target: Path
    logs: Path
    log: Path
    marker: Path


def build_driver(
    zed_source: Path,
    cache_dir: Path,
    *,
    cargo: str = "cargo",
    jobs: int = 4,
) -> BuiltDriver:
    """Build (or reuse) the ``bend2_lint_demo`` driver for ``zed_source``.

    ``zed_source`` is only read: it must be the root of a git checkout with the
    genuine windowless API. ``cache_dir`` is caller-owned and holds the isolated
    clone (``source/``), its build target (``target/``) and ``marker.json``.
    """
    if not isinstance(cargo, str) or not cargo.strip():
        raise BuildDriverError(
            f"cargo must be a non-empty executable name or path, got {cargo!r}"
        )
    if isinstance(jobs, bool) or not isinstance(jobs, int) or jobs < 1:
        raise BuildDriverError(f"jobs must be a positive integer, got {jobs!r}")

    source_root, revision = _inspect_source(Path(zed_source))
    files, digests = _source_assets()
    paths = _cache_paths(Path(cache_dir), source_root)
    marker = _read_marker(paths)

    cached = _cached_driver(paths, marker, source_root, revision, digests)
    if cached is not None:
        return cached

    if marker is None:
        _require_empty_cache(paths)
        _clone_source(source_root, paths.source, revision)
        _install_assets(paths.source, files, digests)
        _wire_crate_manifest(paths.source / "crates/zed/Cargo.toml")
        _write_marker(
            paths,
            _marker_document(
                source_root, revision, digests, cargo=cargo, jobs=jobs, binary=None
            ),
        )
    else:
        _install_assets(paths.source, files, digests)
        _wire_crate_manifest(paths.source / "crates/zed/Cargo.toml")

    _require_owned_target(paths)
    binary = _driver_binary(paths)
    stale_digest = _existing_binary_digest(binary)
    recorded = marker.get("binary") if marker is not None else None
    recorded_digest = recorded.get("sha256") if isinstance(recorded, dict) else None
    binary = _run_cargo(paths, cargo=cargo, jobs=jobs)
    if (
        stale_digest is not None
        and recorded_digest is not None
        and recorded_digest != stale_digest
        and _sha256_file(binary) == stale_digest
    ):
        raise BuildDriverError(
            f"the driver at {binary} does not match the cache marker and cargo did not "
            "rebuild it; use a separate cache directory"
        )
    record = {
        "path": binary.relative_to(paths.root).as_posix(),
        "sha256": _sha256_file(binary),
        "bytes": binary.stat().st_size,
    }
    _write_marker(
        paths,
        _marker_document(
            source_root, revision, digests, cargo=cargo, jobs=jobs, binary=record
        ),
    )
    return BuiltDriver(binary=binary, source=paths.source, revision=revision)


def _source_assets() -> tuple[dict[str, Path], dict[str, str]]:
    """Return the bundled driver sources as ``clone-relative path -> file``."""
    if not SOURCE_ASSETS_DIR.is_dir():
        raise BuildDriverError(
            f"bundled driver sources are missing: expected directory {SOURCE_ASSETS_DIR}"
        )
    files: dict[str, Path] = {}
    for path in sorted(SOURCE_ASSETS_DIR.rglob("*")):
        if path.is_symlink():
            raise BuildDriverError(
                f"bundled driver sources must be plain files, found a symlink: {path}"
            )
        if not path.is_file():
            continue
        relative = path.relative_to(SOURCE_ASSETS_DIR).as_posix()
        if not relative.startswith(_SOURCE_ASSET_PREFIX):
            raise BuildDriverError(
                f"bundled driver source {relative} must live under {_SOURCE_ASSET_PREFIX} "
                f"inside {SOURCE_ASSETS_DIR}"
            )
        files[relative] = path
    missing = [relative for relative in REQUIRED_SOURCE_ASSETS if relative not in files]
    if missing:
        raise BuildDriverError(
            "bundled driver sources are incomplete; missing " + ", ".join(missing)
        )
    digests = {relative: _sha256_file(path) for relative, path in files.items()}
    return files, digests


def _inspect_source(zed_source: Path) -> tuple[Path, str]:
    """Validate the supplied checkout and return its root and revision."""
    source = Path(zed_source).expanduser()
    if not source.exists():
        raise BuildDriverError(f"Zed source checkout does not exist: {source}")
    if not source.is_dir():
        raise BuildDriverError(f"Zed source checkout is not a directory: {source}")

    root = Path(_git(source, "rev-parse", "--show-toplevel"))
    if root.resolve() != source.resolve():
        raise BuildDriverError(
            f"Zed source must be the repository root {root}, not {source}"
        )
    if _git(root, "rev-parse", "--is-inside-work-tree") != "true":
        raise BuildDriverError(f"Zed source {root} is not a git work tree")

    revision = _git(root, "rev-parse", "HEAD").lower()
    if not _REVISION.match(revision):
        raise BuildDriverError(
            f"cannot read a commit revision from {root}: got {revision!r}"
        )

    _require_headless_api(root)
    _require_crate_manifest(root)
    return root.resolve(), revision


def _require_headless_api(root: Path) -> None:
    for relative, markers in _HEADLESS_API_MARKERS:
        path = root / relative
        if path.is_symlink() or not path.is_file():
            raise _unsupported_source(root, f"{relative} is missing")
        text = path.read_text(encoding="utf-8", errors="replace")
        for marker in markers:
            if marker not in text:
                raise _unsupported_source(root, f"{relative} does not contain {marker!r}")


def _require_crate_manifest(root: Path) -> None:
    manifest_path = root / "crates/zed/Cargo.toml"
    if not (root / "crates/zed/src/main.rs").is_file():
        raise _unsupported_source(root, "crates/zed/src/main.rs is missing")
    try:
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise _unsupported_source(root, f"crates/zed/Cargo.toml is unreadable: {error}")

    package = manifest.get("package")
    if not isinstance(package, dict) or package.get("name") != DRIVER_CRATE:
        raise _unsupported_source(root, "crates/zed/Cargo.toml is not the zed crate")
    features = manifest.get("features")
    visual_tests = features.get("visual-tests") if isinstance(features, dict) else None
    if not isinstance(visual_tests, list) or "gpui_platform/screen-capture" not in visual_tests:
        raise _unsupported_source(
            root, "the zed crate has no visual-tests feature with screenshot capture"
        )

    try:
        workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise _unsupported_source(root, f"Cargo.toml is unreadable: {error}")
    workspace_table = workspace.get("workspace")
    workspace_dependencies = (
        workspace_table.get("dependencies", {})
        if isinstance(workspace_table, dict)
        else {}
    )
    if not isinstance(workspace_dependencies, dict):
        raise _unsupported_source(root, "the workspace has no [workspace.dependencies]")
    missing = [name for name in _WORKSPACE_DEPENDENCIES if name not in workspace_dependencies]
    if missing:
        raise _unsupported_source(
            root,
            "the workspace has no " + ", ".join(missing) + " dependency",
        )


def _unsupported_source(root: Path, detail: str) -> BuildDriverError:
    return BuildDriverError(
        f"{root} cannot host the bend2 lint capture driver: {detail}. "
        f"The tested and supported Zed source pin is {TESTED_ZED_REVISION} "
        f"(Zed {TESTED_ZED_VERSION} with the windowless Metal renderer)."
    )


def _git(root: Path, *arguments: str) -> str:
    try:
        completed = subprocess.run(
            ["git", "-C", str(root), *arguments],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    except OSError as error:
        raise BuildDriverError(f"cannot run git: {error}") from error
    if completed.returncode != 0:
        message = completed.stderr.strip() or completed.stdout.strip()
        raise BuildDriverError(
            f"git {' '.join(arguments)} failed in {root}: {message}"
        )
    return completed.stdout.strip()


def _cache_paths(cache_dir: Path, source_root: Path) -> _CachePaths:
    cache = Path(cache_dir).expanduser()
    if cache.is_symlink():
        raise BuildDriverError(f"build cache path is a symlink: {cache}")
    if cache.exists() and not cache.is_dir():
        raise BuildDriverError(f"build cache path is not a directory: {cache}")
    root = cache.resolve()
    resolved_source = source_root.resolve()
    if root == resolved_source or resolved_source in root.parents:
        raise BuildDriverError(
            f"build cache {root} must live outside the Zed source checkout {resolved_source}"
        )
    try:
        root.mkdir(parents=True, exist_ok=True)
    except OSError as error:
        raise BuildDriverError(f"cannot create build cache {root}: {error}") from error

    paths = _CachePaths(
        root=root,
        source=root / "source",
        target=root / "target",
        logs=root / "logs",
        log=root / "logs" / "cargo-build.log",
        marker=root / MARKER_FILENAME,
    )
    for path in (paths.source, paths.target, paths.logs, paths.marker):
        if path.is_symlink():
            raise BuildDriverError(
                f"build cache entry {path} is a symlink; the cache must be owned by this tool"
            )
        if path.exists() and root not in path.resolve().parents:
            raise BuildDriverError(
                f"build cache entry {path} escapes the cache directory {root}"
            )
    return paths


def _require_empty_cache(paths: _CachePaths) -> None:
    occupied = sorted(path.name for path in paths.root.iterdir())
    if occupied:
        raise BuildDriverError(
            f"build cache {paths.root} is not owned by this tool: it already contains "
            f"{', '.join(occupied)} but no {MARKER_FILENAME}. "
            "Choose another cache directory; this tool never repairs foreign caches."
        )


def _require_owned_target(paths: _CachePaths) -> None:
    """Refuse to build into a target directory this tool did not create."""
    target = paths.target
    if not target.exists():
        return
    if target.is_symlink() or not target.is_dir():
        raise BuildDriverError(
            f"build target {target} is not a directory; use a separate cache directory"
        )
    owned = (
        (target / "CACHEDIR.TAG").is_file()
        or (target / ".rustc_info.json").is_file()
        or (target / "debug").is_dir()
    )
    if not owned and any(target.iterdir()):
        raise BuildDriverError(
            f"build target {target} is not owned by this tool (no Cargo target "
            "metadata); use a separate cache directory"
        )


def _read_marker(paths: _CachePaths) -> dict | None:
    if not paths.marker.exists():
        return None
    if paths.marker.is_symlink() or not paths.marker.is_file():
        raise BuildDriverError(f"build cache marker is not a plain file: {paths.marker}")
    try:
        document = json.loads(paths.marker.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise BuildDriverError(
            f"build cache marker {paths.marker} is unreadable: {error}"
        ) from error
    if not isinstance(document, dict) or document.get("schema") != CACHE_SCHEMA:
        raise BuildDriverError(
            f"{paths.marker} was not written by this tool (expected schema "
            f"{CACHE_SCHEMA}); choose another cache directory"
        )
    return document


def _cached_driver(
    paths: _CachePaths,
    marker: dict | None,
    source_root: Path,
    revision: str,
    digests: dict[str, str],
) -> BuiltDriver | None:
    """Return a reusable driver, or ``None`` when a refresh and rebuild are needed."""
    if marker is None:
        return None
    expected_source = str(source_root)
    cached_source = marker.get("zed_source")
    if cached_source != expected_source:
        raise BuildDriverError(
            f"build cache {paths.root} belongs to Zed source {cached_source!r}, "
            f"not {expected_source!r}; use one cache directory per checkout"
        )
    cached_revision = marker.get("source_revision")
    if cached_revision != revision:
        raise BuildDriverError(
            f"build cache {paths.root} was built from revision {cached_revision}; "
            f"the supplied checkout is at {revision}. Use a separate cache directory; "
            "this tool never rebuilds over a conflicting cache."
        )
    if paths.source.is_symlink() or not paths.source.is_dir():
        raise BuildDriverError(
            f"build cache {paths.root} is missing its isolated clone {paths.source}; "
            "use a separate cache directory"
        )
    if not (paths.source / ".git").exists():
        raise BuildDriverError(
            f"the cached clone {paths.source} is not a git work tree; "
            "use a separate cache directory"
        )
    actual = _git(paths.source, "rev-parse", "HEAD").lower()
    if actual != revision:
        raise BuildDriverError(
            f"the cached clone {paths.source} is at revision {actual}, not {revision}; "
            "use a separate cache directory"
        )

    # Changed bundled driver sources are normal iteration inside an owned cache:
    # the refresh path reinstalls the known driver files and reuses the target.
    if marker.get("assets") != digests:
        return None
    if not paths.target.is_dir():
        return None
    for relative, digest in digests.items():
        path = paths.source / relative
        if path.is_symlink():
            raise BuildDriverError(
                f"the cached driver source {path} is a symlink; use a separate cache "
                "directory"
            )
        if not path.is_file() or _sha256_file(path) != digest:
            return None
    _wire_crate_manifest(paths.source / "crates/zed/Cargo.toml")

    record = marker.get("binary")
    if not isinstance(record, dict):
        return None
    binary = _driver_binary(paths)
    if binary.is_symlink() or not binary.is_file():
        return None
    if _sha256_file(binary) != record.get("sha256"):
        return None
    if binary.stat().st_size != record.get("bytes"):
        return None
    if os.name != "nt" and not os.access(binary, os.X_OK):
        return None
    return BuiltDriver(binary=binary, source=paths.source, revision=revision)


def _clone_source(source_root: Path, destination: Path, revision: str) -> None:
    if destination.exists() or destination.is_symlink():
        raise BuildDriverError(f"refusing to clone over an existing path: {destination}")
    try:
        destination.parent.mkdir(parents=True, exist_ok=True)
        completed = subprocess.run(
            [
                "git",
                "clone",
                "--no-hardlinks",
                "--no-checkout",
                "--quiet",
                str(source_root),
                str(destination),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if completed.returncode != 0:
            message = completed.stderr.strip() or completed.stdout.strip()
            raise BuildDriverError(
                f"cannot clone {source_root} into {destination}: {message}"
            )
        _git(
            destination,
            "-c",
            "advice.detachedHead=false",
            "checkout",
            "--detach",
            "--force",
            revision,
        )
        actual = _git(destination, "rev-parse", "HEAD").lower()
        if actual != revision:
            raise BuildDriverError(
                f"the isolated clone {destination} is at revision {actual}, not {revision}"
            )
        alternates = destination / ".git" / "objects" / "info" / "alternates"
        if alternates.exists() or alternates.is_symlink():
            raise BuildDriverError(
                f"the isolated clone {destination} shares git objects with {source_root}"
            )
    except BaseException:
        if destination.is_dir() and not destination.is_symlink():
            shutil.rmtree(destination, ignore_errors=True)
        raise


def _install_assets(
    clone: Path, files: dict[str, Path], digests: dict[str, str]
) -> None:
    """Install or refresh the owned driver sources inside the isolated clone."""
    for relative, asset in files.items():
        destination = clone / relative
        if clone.resolve() not in destination.resolve().parents:
            raise BuildDriverError(
                f"bundled driver source {relative} would escape the clone {clone}"
            )
        if destination.is_symlink():
            raise BuildDriverError(
                f"refusing to replace the symlinked driver source {destination}; "
                "use a separate cache directory"
            )
        if destination.exists() and not destination.is_file():
            raise BuildDriverError(
                f"refusing to replace the non-regular driver source {destination}; "
                "use a separate cache directory"
            )
        if destination.is_file() and _sha256_file(destination) == digests[relative]:
            continue

        temporary = destination.with_name(destination.name + ".install-tmp")
        try:
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(asset, temporary)
            temporary.chmod(0o644)
            os.replace(temporary, destination)
        except OSError as error:
            try:
                temporary.unlink(missing_ok=True)
            except OSError:
                pass
            raise BuildDriverError(
                f"cannot install the bundled driver source {relative} into {clone}: {error}"
            ) from error
        if _sha256_file(destination) != digests[relative]:
            raise BuildDriverError(
                f"installing the bundled driver source {relative} produced different bytes"
            )


def _wire_crate_manifest(manifest: Path) -> None:
    """Idempotently add the driver target and its dependencies to the manifest."""
    try:
        text = manifest.read_text(encoding="utf-8")
    except OSError as error:
        raise BuildDriverError(f"cannot read {manifest}: {error}") from error
    patched = _patched_manifest_text(text, manifest)
    if patched == text:
        return
    try:
        document = tomllib.loads(patched)
    except tomllib.TOMLDecodeError as error:
        raise BuildDriverError(
            f"patching {manifest} produced invalid TOML: {error}"
        ) from error
    _verify_manifest(document, manifest)
    _atomic_write_text(manifest, patched)


def _patched_manifest_text(text: str, manifest: Path) -> str:
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise BuildDriverError(f"{manifest} is not valid TOML: {error}") from error

    bins = document.get("bin", [])
    if not isinstance(bins, list):
        raise BuildDriverError(
            f"{manifest} has a malformed bin table; expected [[bin]] entries"
        )
    matching = [
        entry
        for entry in bins
        if isinstance(entry, dict) and entry.get("name") == DRIVER_BIN_NAME
    ]
    if len(matching) > 1:
        raise BuildDriverError(
            f"{manifest} defines {DRIVER_BIN_NAME!r} more than once"
        )
    lines = text.splitlines(keepends=True)
    if matching:
        expected = {
            "path": DRIVER_MANIFEST_PATH,
            "required-features": [DRIVER_FEATURES],
        }
        conflicts = [
            f"{key} = {matching[0].get(key)!r}"
            for key, value in expected.items()
            if matching[0].get(key) != value
        ]
        if conflicts:
            raise BuildDriverError(
                f"{manifest} already defines a conflicting [[bin]] "
                f"{DRIVER_BIN_NAME!r} ({', '.join(conflicts)}); refuse to rewrite it"
            )
    else:
        lines = _insert_bin_block(lines, manifest)

    dependencies = document.get("dependencies")
    if not isinstance(dependencies, dict):
        raise BuildDriverError(
            f"{manifest} has no [dependencies] table to wire the driver into"
        )
    for name in _WORKSPACE_DEPENDENCIES:
        if name in dependencies:
            spec = dependencies[name]
            if spec != {"workspace": True}:
                raise BuildDriverError(
                    f"{manifest} already declares {name!r} as {spec!r}; the driver needs "
                    f"an exact `{name}.workspace = true` dependency, not a replacement"
                )
            continue
        lines = _insert_dependency_line(lines, name, manifest)
    return "".join(lines)


def _insert_bin_block(lines: list[str], manifest: Path) -> list[str]:
    anchor = _section_start(lines, "[dependencies]")
    if anchor is None:
        raise BuildDriverError(
            f"{manifest} has no [dependencies] table to place the driver target before"
        )
    block = list(_DRIVER_BIN_LINES)
    if anchor == 0 or lines[anchor - 1].strip():
        block.insert(0, "\n")
    return lines[:anchor] + block + lines[anchor:]


def _insert_dependency_line(lines: list[str], name: str, manifest: Path) -> list[str]:
    header = _section_start(lines, "[dependencies]")
    if header is None:
        raise BuildDriverError(f"{manifest} has no [dependencies] table")
    end = len(lines)
    for index in range(header + 1, len(lines)):
        stripped = lines[index].lstrip()
        if stripped.startswith("[") and not lines[index][:1].isspace():
            end = index
            break

    insertion = end
    for index in range(header + 1, end):
        match = _DEPENDENCY_KEY.match(lines[index])
        if match and match.group(1) > name:
            insertion = index
            break

    new_line = f"{name}.workspace = true\n"
    if insertion == len(lines) and lines and not lines[-1].endswith("\n"):
        lines[-1] = lines[-1] + "\n"
    return lines[:insertion] + [new_line] + lines[insertion:]


def _section_start(lines: list[str], header: str) -> int | None:
    for index, line in enumerate(lines):
        if line.rstrip("\r\n") == header:
            return index
    return None


def _verify_manifest(document: dict, manifest: Path) -> None:
    bins = document.get("bin", [])
    entry = None
    if isinstance(bins, list):
        entry = next(
            (
                candidate
                for candidate in bins
                if isinstance(candidate, dict)
                and candidate.get("name") == DRIVER_BIN_NAME
            ),
            None,
        )
    if (
        entry is None
        or entry.get("path") != DRIVER_MANIFEST_PATH
        or entry.get("required-features") != [DRIVER_FEATURES]
    ):
        raise BuildDriverError(
            f"internal wiring error: {manifest} lacks the expected [[bin]] "
            f"{DRIVER_BIN_NAME!r}"
        )
    dependencies = document.get("dependencies", {})
    for name in _WORKSPACE_DEPENDENCIES:
        if dependencies.get(name) != {"workspace": True}:
            raise BuildDriverError(
                f"internal wiring error: {manifest} lacks `{name}.workspace = true`"
            )


def _run_cargo(paths: _CachePaths, *, cargo: str, jobs: int) -> Path:
    _require_toolchain(cargo, paths.root)
    environment = os.environ.copy()
    environment["CARGO_TARGET_DIR"] = str(paths.target)
    environment["CARGO_INCREMENTAL"] = "0"
    environment["CARGO_PROFILE_DEV_DEBUG"] = "0"
    environment["CARGO_PROFILE_DEV_SPLIT_DEBUGINFO"] = "off"
    environment["RUSTUP_AUTO_INSTALL"] = "0"
    command = [
        cargo,
        f"+{RUST_TOOLCHAIN}",
        "build",
        "--package",
        DRIVER_CRATE,
        "--bin",
        DRIVER_BIN_NAME,
        "--features",
        DRIVER_FEATURES,
        "--jobs",
        str(jobs),
    ]
    try:
        paths.logs.mkdir(parents=True, exist_ok=True)
        with paths.log.open("w", encoding="utf-8") as log:
            log.write("$ " + " ".join(command) + f"\n# cwd {paths.source}\n")
            log.flush()
            with owned_process(
                command,
                cwd=paths.source,
                env=environment,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1,
            ) as process:
                stream = process.stdout
                if stream is None:
                    raise BuildDriverError(
                        f"cargo produced no output stream for {paths.source}"
                    )
                for line in stream:
                    log.write(line)
                    log.flush()
                    sys.stderr.write(line)
                    sys.stderr.flush()
                status = process.wait()
    except OSError as error:
        raise BuildDriverError(f"cannot run cargo ({cargo!r}): {error}") from error

    if status != 0:
        raise BuildDriverError(
            f"cargo build of {DRIVER_BIN_NAME} failed with exit status {status}; "
            f"the full log is {paths.log}"
        )
    binary = _driver_binary(paths)
    if binary.is_symlink() or not binary.is_file():
        raise BuildDriverError(
            f"cargo reported success but {binary} does not exist; the build cache is unusable"
        )
    if binary.stat().st_size == 0:
        raise BuildDriverError(f"cargo produced an empty driver at {binary}")
    if os.name != "nt" and not os.access(binary, os.X_OK):
        raise BuildDriverError(f"cargo produced a non-executable driver at {binary}")
    return binary


def _require_toolchain(cargo: str, workdir: Path) -> None:
    """Require the pinned toolchain to be installed already; never provision it.

    ``workdir`` must not contain a ``rust-toolchain`` override file, so rustup has
    no reason to consider installing anything for the selection check.
    """
    environment = os.environ.copy()
    environment["RUSTUP_AUTO_INSTALL"] = "0"
    rustup = shutil.which("rustup")
    if rustup is None and os.sep in cargo:
        sibling = Path(cargo).with_name("rustup")
        if sibling.is_file():
            rustup = str(sibling)
    if rustup is None:
        raise BuildDriverError(
            f"rustup is required to select the pinned Rust toolchain {RUST_TOOLCHAIN} "
            f"through {cargo!r}, but no rustup executable is available. Install rustup "
            f"and run `rustup toolchain install {RUST_TOOLCHAIN}` yourself; this tool "
            "never installs toolchains."
        )

    try:
        listed = subprocess.run(
            [rustup, "toolchain", "list"],
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    except OSError as error:
        raise BuildDriverError(f"cannot run rustup: {error}") from error
    if listed.returncode != 0:
        detail = (listed.stderr.strip() or listed.stdout.strip()).splitlines()
        raise BuildDriverError(
            f"`rustup toolchain list` failed{(': ' + detail[-1]) if detail else ''}"
        )
    installed = [
        line.split()[0] for line in listed.stdout.splitlines() if line.strip()
    ]
    if not any(
        name == RUST_TOOLCHAIN or name.startswith(RUST_TOOLCHAIN + "-")
        for name in installed
    ):
        raise BuildDriverError(
            f"the pinned Rust toolchain {RUST_TOOLCHAIN} is not installed (rustup "
            f"reports: {', '.join(installed) or 'none'}). Run "
            f"`rustup toolchain install {RUST_TOOLCHAIN}` explicitly; this tool never "
            "installs toolchains."
        )

    try:
        selected = subprocess.run(
            [cargo, f"+{RUST_TOOLCHAIN}", "--version"],
            cwd=workdir,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
    except OSError as error:
        raise BuildDriverError(f"cannot run cargo ({cargo!r}): {error}") from error
    if selected.returncode != 0:
        detail = (selected.stderr.strip() or selected.stdout.strip()).splitlines()
        raise BuildDriverError(
            f"cargo cannot select the pinned Rust toolchain {RUST_TOOLCHAIN} through "
            f"{cargo!r}{(': ' + detail[-1]) if detail else ''}. Install it explicitly "
            f"with `rustup toolchain install {RUST_TOOLCHAIN}`; this tool never installs "
            "toolchains."
        )
    reported = selected.stdout.strip().split()
    if len(reported) < 2 or reported[1] != RUST_TOOLCHAIN:
        raise BuildDriverError(
            f"the selected toolchain reports {' '.join(reported) or 'no version'}, not "
            f"cargo {RUST_TOOLCHAIN}; point --cargo at a rustup-managed cargo for the "
            "pinned toolchain"
        )


def _driver_binary(paths: _CachePaths) -> Path:
    executable = DRIVER_BIN_NAME + (".exe" if os.name == "nt" else "")
    return paths.target / "debug" / executable


def _existing_binary_digest(binary: Path) -> str | None:
    if binary.is_symlink() or not binary.is_file():
        return None
    return _sha256_file(binary)


def _marker_document(
    source_root: Path,
    revision: str,
    digests: dict[str, str],
    *,
    cargo: str,
    jobs: int,
    binary: dict | None,
) -> dict:
    return {
        "schema": CACHE_SCHEMA,
        "zed_source": str(source_root),
        "source_revision": revision,
        "tested_source_revision": TESTED_ZED_REVISION,
        "toolchain": RUST_TOOLCHAIN,
        "assets": dict(sorted(digests.items())),
        "cargo": cargo,
        "jobs": jobs,
        "binary": binary,
    }


def _write_marker(paths: _CachePaths, document: dict) -> None:
    _atomic_write_text(
        paths.marker, json.dumps(document, indent=2, sort_keys=True) + "\n"
    )


def _atomic_write_text(path: Path, text: str) -> None:
    temporary = path.with_name(path.name + ".tmp")
    try:
        temporary.write_text(text, encoding="utf-8")
        os.replace(temporary, path)
    except OSError as error:
        raise BuildDriverError(f"cannot write {path}: {error}") from error
    finally:
        try:
            temporary.unlink(missing_ok=True)
        except OSError:
            pass


def _sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()
