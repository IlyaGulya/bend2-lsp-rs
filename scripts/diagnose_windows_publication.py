#!/usr/bin/env python3
"""Independently verify a genuine Windows ARM compiler published in place.

Diagnostic-only: this reads and executes the installation it was given, but
never installs, moves, repairs, or reconfigures it. Every observation is
written to the evidence directory before a failure is raised.
"""

import argparse
import hashlib
import json
import locale
import os
from pathlib import Path
import platform
import subprocess

from bootstrap_bend import PINS, compiler_environment
from bootstrap_bend_source import PUBLICATION_FILE, observe_owned_processes
from diagnose_windows_lint import pe_identity


TARGET = "aarch64-pc-windows-msvc"
VERIFICATION_FILE = "publication-verification.json"


def file_identity(path: Path) -> dict:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"expected a regular file: {path}")
    with path.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    return {"path": str(path), "size": path.stat().st_size, "sha256": digest}


def normalized_text(raw: bytes) -> str:
    # The provisioning probes capture text=True, which decodes with the locale
    # encoding and applies universal newlines. Mirror both so the complete
    # Base output is compared byte-for-byte apart from that documented
    # translation.
    return raw.decode(locale.getpreferredencoding(False)).replace("\r\n", "\n").replace("\r", "\n")


def verify(
    record: dict,
    destination: Path,
    evidence_dir: Path,
    exported_compiler: str | None,
    provision_outcome: str,
) -> None:
    checks = record["checks"]
    checks["provision_outcome"] = provision_outcome
    if provision_outcome != "success":
        raise ValueError(f"compiler provisioning outcome was {provision_outcome}")

    publication = json.loads((destination / PUBLICATION_FILE).read_text(encoding="utf-8"))
    checks["publication"] = publication
    if publication.get("state") != "installed":
        raise ValueError(f"compiler publication state is {publication.get('state')!r}, not 'installed'")
    if (
        os.path.normcase(os.path.abspath(publication.get("destination", "")))
        != os.path.normcase(os.path.abspath(destination))
        or publication.get("target") != TARGET
    ):
        raise ValueError("publication record does not describe this destination and native target")
    if (
        publication.get("bend_version") != PINS["version"]
        or publication.get("source_commit") != PINS["source_commit"]
    ):
        raise ValueError("publication record does not match the pinned compiler source")

    identity = json.loads((destination / "compiler-build-identity.json").read_text(encoding="utf-8"))
    checks["identity"] = identity
    if identity.get("validation", {}).get("status") != "passed":
        raise ValueError(f"compiler identity is not validated: {identity.get('validation')}")
    expected = {
        "bend_version": PINS["version"],
        "source_commit": PINS["source_commit"],
        "source_archive_sha256": PINS["source"]["sha256"],
        "original_locator_sha256": PINS["source"]["files"]["bend2/bend.ts"][1],
        "adjusted_locator_sha256": PINS["source"]["windows_locator"]["patched_file"][1],
        "bun_revision": f"{PINS['bun']['version']}+{PINS['bun']['revision'][:9]}",
        "bun_host": "win32:arm64",
        "bun_asset": PINS["bun"]["archives"][TARGET]["asset"],
        "bun_archive_sha256": PINS["bun"]["archives"][TARGET]["sha256"],
        "target": TARGET,
        "embedded_bun_version": PINS["bun"]["version"],
        "embedded_bun_revision": PINS["bun"]["revision"],
        "embedded_host": "win32:arm64",
    }
    for key, value in expected.items():
        if identity.get(key) != value:
            raise ValueError(f"compiler identity {key} is {identity.get(key)!r}, expected {value!r}")

    executable = destination / "bin/bend.exe"
    checks["executable"] = file_identity(executable)
    if (
        checks["executable"]["size"] != identity.get("executable_size")
        or checks["executable"]["sha256"] != identity.get("executable_sha256")
    ):
        raise ValueError("installed compiler bytes do not match the validated identity")
    if Path(identity["embedded_process"]["exec_path"]).resolve() != executable.resolve():
        raise ValueError("embedded runtime identity does not describe the installed executable")

    native = pe_identity(executable)
    checks["native_pe"] = native
    if native["machine"] != "0xaa64" or native["sha256"] != identity["executable_sha256"]:
        raise ValueError("installed compiler is not the validated native ARM64 image")

    embedded = destination / "compiler-embedded-source.js"
    checks["embedded_source"] = file_identity(embedded)
    if checks["embedded_source"]["sha256"] != identity.get("embedded_source_sha256"):
        raise ValueError("embedded source bytes do not match the validated identity")

    resources = {}
    for relative, (size, digest) in PINS["resources"].items():
        observed = file_identity(destination / relative)
        if observed["size"] != size or observed["sha256"] != digest:
            raise ValueError(f"compiler resource does not match its pin: {relative}")
        resources[relative] = observed
    for relative in ("bend2/effs", "guide"):
        directory = destination / relative
        if directory.is_symlink() or not directory.is_dir():
            raise ValueError(f"compiler resource directory is missing: {relative}")
        resources[relative] = {"path": str(directory), "kind": "directory"}
    checks["resources"] = resources

    # GITHUB_ENV names a different command file in each step. Check the actual
    # environment inherited by this independent post-provisioning process.
    checks["exported_environment"] = {
        "BEND2_COMPILER_TEST_PATH": exported_compiler,
        "BEND_NO_TELEMETRY": os.environ.get("BEND_NO_TELEMETRY"),
    }
    if not exported_compiler or (
        os.path.normcase(os.path.abspath(exported_compiler))
        != os.path.normcase(os.path.abspath(executable))
    ):
        raise ValueError(f"BEND2_COMPILER_TEST_PATH does not name the installed compiler: {exported_compiler!r}")
    if os.environ.get("BEND_NO_TELEMETRY") != "1":
        raise ValueError("BEND_NO_TELEMETRY was not available to later tests")

    environment = compiler_environment()
    environment["BEND_NO_TELEMETRY"] = "1"
    probe_cwd = evidence_dir / "probe-cwd"
    probe_cwd.mkdir(exist_ok=True)
    checks["probes"] = {}
    for name, arguments in (("version", ["version"]), ("base", ["base", "--types"])):
        completed = subprocess.run(
            [str(executable), *arguments], cwd=probe_cwd, env=environment,
            capture_output=True, timeout=30,
        )
        captured = identity["cli_checks"][name]
        (evidence_dir / f"probe-{name}.stdout").write_bytes(completed.stdout)
        (evidence_dir / f"probe-{name}.stderr").write_bytes(completed.stderr)
        stdout_matches = normalized_text(completed.stdout) == captured["stdout"]
        stderr_matches = normalized_text(completed.stderr) == captured["stderr"]
        checks["probes"][name] = {
            "returncode": completed.returncode,
            "stdout_size": len(completed.stdout),
            "stdout_sha256": hashlib.sha256(completed.stdout).hexdigest(),
            "stderr_size": len(completed.stderr),
            "stderr_sha256": hashlib.sha256(completed.stderr).hexdigest(),
            "stdout_matches_validation": stdout_matches,
            "stderr_matches_validation": stderr_matches,
        }
        if completed.returncode != captured["returncode"]:
            raise ValueError(f"installed compiler {name} probe returned {completed.returncode}")
        if not stdout_matches:
            raise ValueError(f"installed compiler {name} output differs from the validated bytes")
        if not stderr_matches:
            raise ValueError(f"installed compiler {name} stderr differs from the validated bytes")
        if name == "base" and not completed.stdout:
            raise ValueError("installed compiler returned no Base bytes")

    checks["post_probe_executable"] = file_identity(executable)
    if checks["post_probe_executable"]["sha256"] != identity["executable_sha256"]:
        raise ValueError("installed compiler bytes changed during verification")

    ownership = observe_owned_processes((destination,))
    checks["process_ownership"] = ownership
    if ownership["live_owned"]:
        raise ValueError(f"live owned compiler processes remain after verification: {ownership['live_owned']}")

    checks["host"] = {
        "system": platform.system(),
        "machine": platform.machine(),
        "python": platform.python_version(),
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--destination", required=True, type=Path)
    parser.add_argument("--evidence-dir", required=True, type=Path)
    parser.add_argument("--provision-outcome", required=True)
    arguments = parser.parse_args()
    destination = arguments.destination.absolute()
    evidence_dir = arguments.evidence_dir.absolute()
    evidence_dir.mkdir(parents=True, exist_ok=True)
    record = {
        "destination": str(destination),
        "target": TARGET,
        "verified": False,
        "checks": {},
    }
    try:
        verify(record, destination, evidence_dir, os.environ.get("BEND2_COMPILER_TEST_PATH"), arguments.provision_outcome)
    except BaseException as error:
        record["error"] = str(error)[:4000]
        (evidence_dir / VERIFICATION_FILE).write_text(
            json.dumps(record, indent=2) + "\n", encoding="utf-8",
        )
        raise
    record["verified"] = True
    (evidence_dir / VERIFICATION_FILE).write_text(
        json.dumps(record, indent=2) + "\n", encoding="utf-8",
    )
    print(f"Verified native {TARGET} compiler published in place: {destination / 'bin/bend.exe'}")


if __name__ == "__main__":
    main()
