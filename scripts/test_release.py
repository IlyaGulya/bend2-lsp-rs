import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from release import TARGETS, validate_artifacts, verify


VERSION = "1.2.3"
TAG = f"v{VERSION}"


class ReleaseIntegrityTests(unittest.TestCase):
    def setUp(self):
        temporary = self.enterContext(tempfile.TemporaryDirectory())
        self.root = Path(temporary)
        original = Path.cwd()
        os.chdir(self.root)
        self.addCleanup(os.chdir, original)
        self.enterContext(patch.dict(os.environ, {
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_DATE": "2000-01-01T00:00:00+00:00",
            "GIT_COMMITTER_DATE": "2000-01-01T00:00:00+00:00",
        }))
        Path("Cargo.toml").write_text(f'[package]\nname = "bend2-lsp"\nversion = "{VERSION}"\n')
        self.write_lock(VERSION)
        self.write_manifest(VERSION)
        # A throwaway repository lets source verification exercise real git HEAD,
        # without mocking the SHA or interacting with the developer's checkout.
        self.git("init", "--quiet")
        self.git("add", "Cargo.toml", "Cargo.lock", ".release-please-manifest.json")
        self.git(
            "-c", "user.name=Release fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "-c", "core.hooksPath=.git/hooks",
            "commit", "--quiet", "-m", "Fixture",
        )
        self.sha = self.git("rev-parse", "HEAD").strip()
        self.enterContext(patch.dict(os.environ, {
            "RELEASE_SHA": self.sha,
            "RELEASE_TAG": TAG,
            "RELEASE_CHANNEL": "stable",
        }))
        Path("dist").mkdir()

    @staticmethod
    def git(*arguments):
        return subprocess.check_output(["git", *arguments], text=True, stderr=subprocess.PIPE)

    @staticmethod
    def write_lock(version):
        Path("Cargo.lock").write_text(
            f'version = 4\n\n[[package]]\nname = "bend2-lsp"\nversion = "{version}"\n'
        )

    @staticmethod
    def write_manifest(version):
        Path(".release-please-manifest.json").write_text(json.dumps({".": version}))

    def write_archive(self, archive_target, **metadata_changes):
        metadata = {
            "source_sha": self.sha,
            "tag": TAG,
            "version": VERSION,
            "target": archive_target,
            "channel": "stable",
        }
        metadata.update(metadata_changes)
        root = f"bend2-lsp-{TAG}-{archive_target}"
        windows = "windows" in archive_target
        name = f"{root}.zip" if windows else f"{root}.tar.gz"
        archive = Path("dist") / name
        executable = "bend2-lsp.exe" if windows else "bend2-lsp"
        files = {
            executable: b"fixture executable bytes",
            "LICENSE": b"fixture license",
            "README.md": b"fixture installation notes",
            "RELEASE-METADATA.json": json.dumps(metadata).encode(),
        }
        if windows:
            with zipfile.ZipFile(archive, "w") as output:
                for filename, content in files.items():
                    member = zipfile.ZipInfo(f"{root}/{filename}", (1980, 1, 1, 0, 0, 0))
                    output.writestr(member, content)
        else:
            with tarfile.open(archive, "w:gz") as output:
                for filename, content in files.items():
                    member = tarfile.TarInfo(f"{root}/{filename}")
                    member.size = len(content)
                    output.addfile(member, io.BytesIO(content))
        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
        Path(f"{archive}.sha256").write_text(f"{checksum}  {name}\n")
        return archive

    def write_all_archives(self):
        return [self.write_archive(target) for target in TARGETS]

    def test_complete_release_emits_checksums_for_each_verified_archive(self):
        archives = self.write_all_archives()
        assets = validate_artifacts(TAG, VERSION)
        expected_checksums = {
            f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}"
            for archive in archives
        }
        self.assertEqual(set(Path("dist/SHA256SUMS").read_text().splitlines()), expected_checksums)
        self.assertEqual(set(assets), {
            "SHA256SUMS",
            *[archive.name for archive in archives],
            *[f"{archive.name}.sha256" for archive in archives],
        })

    def test_corrupted_archive_is_rejected_before_checksum_manifest(self):
        archive = self.write_all_archives()[0]
        with archive.open("ab") as output:
            output.write(b"corruption after packaging")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            validate_artifacts(TAG, VERSION)
        self.assertFalse(Path("dist/SHA256SUMS").exists())

    def test_missing_native_target_cannot_form_a_release(self):
        archives = self.write_all_archives()
        archives[-1].unlink()
        with self.assertRaises(FileNotFoundError):
            validate_artifacts(TAG, VERSION)
        self.assertFalse(Path("dist/SHA256SUMS").exists())

    def test_metadata_identity_mismatch_rejected_despite_valid_checksum(self):
        self.write_all_archives()
        mismatches = {
            "source_sha": "f" * 40,
            "tag": "v9.9.9",
            "version": "9.9.9",
            "target": "aarch64-unknown-linux-gnu",
            "channel": "nightly",
        }
        for field, wrong_value in mismatches.items():
            with self.subTest(field=field):
                self.write_archive(TARGETS[0], **{field: wrong_value})
                with self.assertRaisesRegex(ValueError, "identity mismatch"):
                    validate_artifacts(TAG, VERSION)
                self.assertFalse(Path("dist/SHA256SUMS").exists())

    def test_version_agreement_accepts_the_stable_cargo_version(self):
        self.assertEqual(verify(), VERSION)

    def test_stable_manifest_version_mismatch_is_rejected(self):
        self.write_manifest("1.2.4")
        with self.assertRaisesRegex(ValueError, "versions disagree"):
            verify()

    def test_stable_lock_version_mismatch_is_rejected(self):
        self.write_lock("1.2.4")
        with self.assertRaisesRegex(ValueError, "versions disagree"):
            verify()

    def test_stable_tag_version_mismatch_is_rejected(self):
        with patch.dict(os.environ, {"RELEASE_TAG": "v1.2.4"}):
            with self.assertRaisesRegex(ValueError, "stable tag must match"):
                verify()

    def test_nightly_tag_must_identify_the_exact_source_sha(self):
        with patch.dict(os.environ, {
            "RELEASE_CHANNEL": "nightly",
            "RELEASE_TAG": f"nightly-2026-10-01-{'f' * 40}",
        }):
            with self.assertRaisesRegex(ValueError, "exact source SHA"):
                verify()


if __name__ == "__main__":
    unittest.main()
