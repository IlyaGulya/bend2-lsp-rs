#!/usr/bin/env python3
"""Consumer-visible safety boundaries for the native bend-lint E2E bootstrap."""

import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch

import bootstrap_bend_lint as bootstrap


def archive(entries):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as packaged:
        for name, kind in entries:
            entry = tarfile.TarInfo(name)
            entry.type = kind
            if kind == tarfile.SYMTYPE:
                entry.linkname = "../../escaped"
            content = b"genuine archive content"
            if kind == tarfile.REGTYPE:
                entry.size = len(content)
            packaged.addfile(entry, io.BytesIO(content) if entry.isfile() else None)
    return output.getvalue()


class BootstrapTests(unittest.TestCase):
    def test_valid_package_preserves_nested_file_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "package"
            bootstrap.extract_package(
                archive([("package/src/linter/seam.ts", tarfile.REGTYPE)]), root
            )
            self.assertEqual((root / "src/linter/seam.ts").read_bytes(), b"genuine archive content")

    def test_unsafe_archive_is_rejected_before_any_write(self):
        for invalid in (
            [("package/../../escaped", tarfile.REGTYPE)],
            [("package/src/linked", tarfile.SYMTYPE)],
            [("other/src/linter/seam.ts", tarfile.REGTYPE)],
            [("package/CON", tarfile.REGTYPE)],
            [("package/a:b", tarfile.REGTYPE)],
            [("package/a.", tarfile.REGTYPE)],
            [("package/a\\b", tarfile.REGTYPE)],
            [("package/src/a", tarfile.REGTYPE), ("package/src/a", tarfile.REGTYPE)],
            [("package/src/A", tarfile.REGTYPE), ("package/src/a", tarfile.REGTYPE)],
            [("package/src", tarfile.REGTYPE), ("package/src/a", tarfile.REGTYPE)],
        ):
            with self.subTest(invalid=invalid), tempfile.TemporaryDirectory() as directory:
                root = Path(directory) / "package"
                with self.assertRaises(ValueError):
                    bootstrap.extract_package(archive(invalid), root)
                self.assertFalse(root.exists())
                self.assertFalse((Path(directory) / "escaped").exists())

    def test_existing_destination_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            sentinel = root / "keep"
            sentinel.write_bytes(b"user data")
            with self.assertRaises(ValueError):
                bootstrap.extract_package(archive([("package/keep", tarfile.REGTYPE)]), root)
            self.assertEqual(sentinel.read_bytes(), b"user data")

    def test_non_native_target_is_rejected_before_network(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(bootstrap, "native_target", return_value="aarch64-apple-darwin"):
                with patch.object(bootstrap, "download_asset") as download:
                    with self.assertRaises(ValueError):
                        bootstrap.install(Path(directory) / "install", "x86_64-apple-darwin")
                    download.assert_not_called()

    def test_identity_mismatch_is_rejected_before_provisioning(self):
        for mismatch in ("version", "bun", "bendRelease", "rule-api", "defaults", "script"):
            with self.subTest(mismatch=mismatch), tempfile.TemporaryDirectory() as directory:
                package = package_at(
                    Path(directory),
                    rules=[] if mismatch == "defaults" else ["layout/format"],
                )
                if mismatch == "version":
                    write_metadata(package, version="0.1.1")
                elif mismatch == "bun":
                    write_metadata(package, bun="1.4.3")
                elif mismatch == "bendRelease":
                    write_metadata(package, bend_release="bendlang/bend/v2.0.37")
                elif mismatch == "rule-api":
                    (package / "src/linter/cli.bend").write_text(
                        "import bend-lint-api@0.1.1.0/linter/lint.bend\n", encoding="utf-8"
                    )
                elif mismatch == "script":
                    (package / "src/linter/seam.ts").unlink()
                with self.assertRaises(ValueError):
                    bootstrap.package_identity(package)


def write_metadata(
    package: Path,
    version: str = "0.2.1",
    bun: str = "1.4.4",
    bend_release: str = "bendlang/bend/v2.0.39",
):
    package.mkdir(parents=True, exist_ok=True)
    (package / "package.json").write_text(
        json.dumps(
            {
                "name": "bend-lint",
                "version": version,
                "engines": {"bun": bun},
                "bendRelease": bend_release,
                "bendWindows": "MattCozendey/bend/" + "0" * 40,
            }
        ),
        encoding="utf-8",
    )


def package_at(root: Path, rules):
    package = root / "package"
    write_metadata(package)
    (package / "src/linter").mkdir(parents=True, exist_ok=True)
    (package / "src/linter/seam.ts").write_text("entry\n", encoding="utf-8")
    (package / "src/linter/defaults.json").write_text(json.dumps(rules), encoding="utf-8")
    (package / "src/linter/cli.bend").write_text(
        "import bend-lint-api@0.2.1.0/linter/lint.bend as Lint\n", encoding="utf-8"
    )
    return package


if __name__ == "__main__":
    unittest.main()
