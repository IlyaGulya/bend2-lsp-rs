#!/usr/bin/env python3
"""Exercise generated installers against a local mirror of the exact native assets."""

from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading

from release import TARGETS, archive_name, asset_name, file_sha256, verify, write_archive


def main():
    test_archive = Path(os.environ["RELEASE_TEST_ARCHIVE"]).resolve(strict=True)
    if not test_archive.is_file():
        raise ValueError("RELEASE_TEST_ARCHIVE must point to a nextest archive file")
    verify()
    target = os.environ["RELEASE_TARGET"]
    if target not in TARGETS:
        raise ValueError("unsupported native installer target")
    dist = Path("dist").resolve()
    windows = "windows" in target
    installer = dist / ("bend2-lsp-installer.ps1" if windows else "bend2-lsp-installer.sh")
    with tempfile.TemporaryDirectory(prefix="bend2-installer-e2e-") as temporary:
        root = Path(temporary)
        installation = root / "installed"
        mirror = root / "mirror"
        mirror.mkdir()
        # A valid archive with different executable bytes must be rejected before
        # extraction, not merely fail because its compression/CRC is malformed.
        altered = root / "altered-binary"
        altered.write_bytes((dist / asset_name(target)).read_bytes() + b"checksum-negative-control")
        write_archive(altered, target, mirror / archive_name(target))
        environment = os.environ.copy()
        environment["BEND2_LSP_UNMANAGED_INSTALL"] = str(installation)
        environment["INSTALLER_NO_MODIFY_PATH"] = "1"
        server = ThreadingHTTPServer(("127.0.0.1", 0), partial(SimpleHTTPRequestHandler, directory=str(mirror)))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            environment["BEND2_LSP_DOWNLOAD_URL"] = f"http://127.0.0.1:{server.server_port}"
            if windows:
                shell = shutil.which("pwsh") or shutil.which("powershell")
                if shell is None:
                    raise RuntimeError("PowerShell is required for the native installer smoke")
                command = [shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", str(installer)]
            else:
                command = ["sh", str(installer)]
            rejected = subprocess.run(command, env=environment, capture_output=True, text=True, timeout=120)
            if rejected.returncode == 0 or "checksum" not in (rejected.stdout + rejected.stderr).lower():
                raise ValueError(f"installer did not reject the altered archive by checksum: {rejected}")
            if installation.exists() and any(installation.iterdir()):
                raise ValueError("checksum failure installed files")
            shutil.copyfile(dist / archive_name(target), mirror / archive_name(target))
            subprocess.run(command, env=environment, check=True, timeout=120)
            binary = installation / ("bend2-lsp.exe" if windows else "bend2-lsp")
            if binary.is_symlink() or not binary.is_file():
                raise ValueError("installer did not produce a regular executable")
            if file_sha256(binary) != file_sha256(dist / asset_name(target)):
                raise ValueError("installed executable differs from the native-tested direct asset")
            # Reuse the existing portable protocol/lifecycle suite; the latency
            # client uses Unix-only pipe select and cannot validate Windows.
            environment["BEND2_LSP_TEST_BINARY"] = str(binary)
            subprocess.run([
                "cargo", "nextest", "run", "--archive-file", str(test_archive),
                "--workspace-remap", str(Path.cwd()), "-E", "binary(release_e2e)", "--profile", "ci",
            ], env=environment, check=True, timeout=600)
            print(f"Native generated installer and installed executable E2E passed: {target}")
        finally:
            server.shutdown()
            thread.join()
            server.server_close()


if __name__ == "__main__":
    main()
