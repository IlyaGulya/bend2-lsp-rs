import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from release import TARGETS, asset_name, package, publish, validate_artifacts, validate_uploaded_assets, verify


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

    def write_asset(self, asset_target, **metadata_changes):
        name = asset_name(os.environ["RELEASE_TAG"], asset_target)
        asset = Path("dist") / name
        asset.write_bytes(f"fixture executable bytes for {asset_target}".encode())
        checksum = hashlib.sha256(asset.read_bytes()).hexdigest()
        metadata = {
            "source_sha": self.sha,
            "tag": os.environ["RELEASE_TAG"],
            "version": VERSION,
            "target": asset_target,
            "channel": os.environ["RELEASE_CHANNEL"],
            "asset": name,
            "sha256": checksum,
        }
        metadata.update(metadata_changes)
        Path(f"{asset}.metadata.json").write_text(json.dumps(metadata))
        Path(f"{asset}.sha256").write_text(f"{checksum}  {name}\n")
        return asset

    def write_all_assets(self):
        return [self.write_asset(target) for target in TARGETS]

    def test_complete_release_emits_checksums_for_each_verified_executable(self):
        executables = self.write_all_assets()
        assets = validate_artifacts(TAG, VERSION)
        expected_checksums = {
            f"{hashlib.sha256(asset.read_bytes()).hexdigest()}  {asset.name}"
            for asset in executables
        }
        self.assertEqual(set(Path("dist/SHA256SUMS").read_text().splitlines()), expected_checksums)
        self.assertEqual(set(assets), {
            "SHA256SUMS",
            *[asset.name for asset in executables],
            *[f"{asset.name}.sha256" for asset in executables],
        })

    def test_corrupted_executable_is_rejected_before_checksum_manifest(self):
        asset = self.write_all_assets()[0]
        with asset.open("ab") as output:
            output.write(b"corruption after packaging")
        with self.assertRaisesRegex(ValueError, "checksum mismatch"):
            validate_artifacts(TAG, VERSION)
        self.assertFalse(Path("dist/SHA256SUMS").exists())

    def test_missing_native_target_cannot_form_a_release(self):
        assets = self.write_all_assets()
        assets[-1].unlink()
        with self.assertRaisesRegex(ValueError, "regular file"):
            validate_artifacts(TAG, VERSION)
        self.assertFalse(Path("dist/SHA256SUMS").exists())

    def test_metadata_identity_mismatch_rejected_despite_valid_checksum(self):
        self.write_all_assets()
        mismatches = {
            "source_sha": "f" * 40,
            "tag": "v9.9.9",
            "version": "9.9.9",
            "target": "aarch64-unknown-linux-gnu",
            "channel": "nightly",
            "asset": asset_name(TAG, TARGETS[1]),
            "sha256": "f" * 64,
        }
        for field, wrong_value in mismatches.items():
            with self.subTest(field=field):
                self.write_asset(TARGETS[0], **{field: wrong_value})
                with self.assertRaisesRegex(ValueError, "identity mismatch"):
                    validate_artifacts(TAG, VERSION)
                self.assertFalse(Path("dist/SHA256SUMS").exists())

    def test_rehashed_modified_executable_still_requires_matching_ci_metadata(self):
        asset = self.write_all_assets()[0]
        asset.write_bytes(b"different executable with a valid adjacent checksum")
        checksum = hashlib.sha256(asset.read_bytes()).hexdigest()
        Path(f"{asset}.sha256").write_text(f"{checksum}  {asset.name}\n")
        with self.assertRaisesRegex(ValueError, "identity mismatch"):
            validate_artifacts(TAG, VERSION)
        self.assertFalse(Path("dist/SHA256SUMS").exists())

    def test_missing_checksum_or_metadata_cannot_form_a_release(self):
        self.write_all_assets()
        for suffix in (".sha256", ".metadata.json"):
            with self.subTest(suffix=suffix):
                self.write_asset(TARGETS[0])
                Path(f"dist/{asset_name(TAG, TARGETS[0])}{suffix}").unlink()
                with self.assertRaisesRegex(ValueError, "regular file"):
                    validate_artifacts(TAG, VERSION)
                self.assertFalse(Path("dist/SHA256SUMS").exists())

    def test_unexpected_public_or_internal_files_are_rejected(self):
        self.write_all_assets()
        for name in ("old-release.tar.gz", "unbound.metadata.json"):
            with self.subTest(name=name):
                extra = Path("dist") / name
                extra.write_bytes(b"unexpected")
                with self.assertRaisesRegex(ValueError, "unexpected files"):
                    validate_artifacts(TAG, VERSION)
                self.assertFalse(Path("dist/SHA256SUMS").exists())
                extra.unlink()

    def test_revalidation_replaces_untrusted_aggregate_manifest(self):
        self.write_all_assets()
        expected = validate_artifacts(TAG, VERSION)
        original = Path("dist/SHA256SUMS").read_text()
        Path("dist/SHA256SUMS").write_text("stale checksums\n")
        self.assertEqual(validate_artifacts(TAG, VERSION), expected)
        self.assertEqual(Path("dist/SHA256SUMS").read_text(), original)

    def test_package_stages_independent_executable_bytes_and_tests_the_final_asset(self):
        binaries = Path("target/release")
        binaries.mkdir(parents=True)
        env_file = self.root / "github-env"
        for target in TARGETS:
            with self.subTest(target=target):
                executable = "bend2-lsp.exe" if "windows" in target else "bend2-lsp"
                source = binaries / executable
                content = f"optimized native executable for {target}".encode()
                source.write_bytes(content)
                env_file.write_text("")
                with patch.dict(os.environ, {
                    "RELEASE_TARGET": target,
                    "GITHUB_ENV": str(env_file),
                }):
                    package()
                asset = Path("dist") / asset_name(TAG, target)
                tested = Path(env_file.read_text().strip().split("=", 1)[1])
                self.assertEqual(tested, asset.resolve())
                self.assertTrue(tested.samefile(asset))
                source.write_bytes(b"later cargo output must not change the release")
                self.assertEqual(tested.read_bytes(), content)
                if os.name != "nt":
                    self.assertEqual(asset.stat().st_mode & 0o777, 0o755)
        assets = validate_artifacts(TAG, VERSION)
        self.assertEqual(set(assets), {
            "SHA256SUMS",
            *[asset_name(TAG, target) for target in TARGETS],
            *[f"{asset_name(TAG, target)}.sha256" for target in TARGETS],
        })

    def test_nonregular_binary_checksum_or_metadata_paths_are_rejected(self):
        self.write_all_assets()
        for suffix in ("", ".sha256", ".metadata.json"):
            with self.subTest(suffix=suffix):
                path = Path(f"dist/{asset_name(TAG, TARGETS[0])}{suffix}")
                path.unlink()
                path.mkdir()
                with self.assertRaisesRegex(ValueError, "regular file"):
                    validate_artifacts(TAG, VERSION)
                self.assertFalse(Path("dist/SHA256SUMS").exists())
                path.rmdir()
                self.write_asset(TARGETS[0])

    @unittest.skipIf(os.name == "nt", "creating symlinks on Windows requires extra runner privileges")
    def test_symlinked_artifacts_and_manifest_cannot_escape_the_verified_directory(self):
        self.write_all_assets()
        for suffix in ("", ".sha256", ".metadata.json"):
            with self.subTest(suffix=suffix):
                path = Path(f"dist/{asset_name(TAG, TARGETS[0])}{suffix}")
                outside = self.root / "outside"
                outside.write_bytes(path.read_bytes())
                path.unlink()
                path.symlink_to(outside)
                with self.assertRaisesRegex(ValueError, "regular file"):
                    validate_artifacts(TAG, VERSION)
                self.assertFalse(Path("dist/SHA256SUMS").exists())
                path.unlink()
                self.write_asset(TARGETS[0])
        outside.write_bytes(b"must not overwrite")
        Path("dist/SHA256SUMS").symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "manifest must be a regular file"):
            validate_artifacts(TAG, VERSION)
        self.assertEqual(outside.read_bytes(), b"must not overwrite")

    @staticmethod
    def uploaded_asset(name):
        path = Path("dist") / name
        return {
            "name": name,
            "digest": f"sha256:{hashlib.sha256(path.read_bytes()).hexdigest()}",
            "size": path.stat().st_size,
        }

    def publication_fixture(self, draft=True, partial=False):
        self.write_all_assets()
        assets = validate_artifacts(os.environ["RELEASE_TAG"], VERSION)
        uploaded_names = assets[:2] if partial else assets
        release = {
            "id": 1,
            "draft": draft,
            "prerelease": os.environ["RELEASE_CHANNEL"] == "nightly",
            "assets": [self.uploaded_asset(name) for name in uploaded_names],
        }
        self.api_writes = []
        self.uploaded_names = []

        def fake_api(endpoint, method="GET", data=None, missing_ok=False):
            if endpoint == "branches/main":
                return {"commit": {"sha": "f" * 40}}
            if endpoint != "releases/1":
                raise AssertionError(f"unexpected release API endpoint: {endpoint}")
            if method != "GET":
                self.api_writes.append((method, data))
                if method == "PATCH":
                    release.update(data)
                else:
                    raise AssertionError(f"unexpected release API method: {method}")
            return release

        def fake_upload(command, check):
            existing = {asset["name"] for asset in release["assets"]}
            paths = command[4:command.index("--repo")]
            for path in paths:
                name = Path(path).name
                if name in existing:
                    raise AssertionError("retry attempted to replace an existing asset")
                self.uploaded_names.append(name)
                release["assets"].append(self.uploaded_asset(name))
                existing.add(name)

        self.enterContext(patch("release.verify", return_value=VERSION))
        self.enterContext(patch("release.tag_sha", return_value=self.sha))
        self.enterContext(patch("release.release_for_tag", return_value=release))
        self.enterContext(patch("release.api", side_effect=fake_api))
        self.enterContext(patch("release.subprocess.run", side_effect=fake_upload))
        self.enterContext(patch.dict(os.environ, {"GITHUB_REPOSITORY": "fixture/repository"}))
        return release, assets

    def test_partial_draft_retry_preserves_existing_bytes_and_publishes_complete_asset_set(self):
        release, assets = self.publication_fixture(partial=True)
        original = [dict(asset) for asset in release["assets"]]
        publish()
        self.assertEqual(release["assets"][:2], original)
        self.assertEqual(set(self.uploaded_names), set(assets) - {item["name"] for item in original})
        self.assertEqual({item["name"] for item in release["assets"]}, set(assets))
        self.assertFalse(release["draft"])
        self.assertEqual(self.api_writes, [("PATCH", {
            "draft": False, "prerelease": False, "make_latest": "true",
        })])

    def test_public_release_retry_leaves_all_assets_and_release_unchanged(self):
        release, _ = self.publication_fixture(draft=False)
        original = json.loads(json.dumps(release))
        publish()
        self.assertEqual(release, original)
        self.assertEqual(self.uploaded_names, [])
        self.assertEqual(self.api_writes, [])

    def test_old_archive_release_is_rejected_without_rewriting_public_assets(self):
        release, _ = self.publication_fixture(draft=False)
        release["assets"][0]["name"] = "old-native-release.tar.gz"
        original = json.loads(json.dumps(release))
        with self.assertRaisesRegex(ValueError, "unexpected"):
            publish()
        self.assertEqual(release, original)
        self.assertEqual(self.uploaded_names, [])
        self.assertEqual(self.api_writes, [])

    def test_retry_rejects_different_existing_bytes_in_both_draft_and_public_release(self):
        release, _ = self.publication_fixture()
        release["assets"][0]["digest"] = f"sha256:{'f' * 64}"
        for draft in (True, False):
            with self.subTest(draft=draft):
                release["draft"] = draft
                with self.assertRaisesRegex(ValueError, "bytes do not match"):
                    publish()
                self.assertEqual(self.uploaded_names, [])
                self.assertEqual(self.api_writes, [])

    def test_uploaded_assets_require_digest_size_and_unique_exact_names(self):
        release, assets = self.publication_fixture()
        first = dict(release["assets"][0])
        for change in ({"digest": None}, {"size": first["size"] + 1}):
            with self.subTest(change=change):
                release["assets"][0] = {**first, **change}
                with self.assertRaisesRegex(ValueError, "bytes do not match"):
                    validate_uploaded_assets(release, assets)
        release["assets"][0] = first
        release["assets"].append(dict(first))
        with self.assertRaisesRegex(ValueError, "duplicate"):
            validate_uploaded_assets(release, assets)

    def test_unexpected_metadata_upload_is_rejected_before_publication(self):
        release, _ = self.publication_fixture()
        release["assets"].append(self.uploaded_asset(f"{asset_name(TAG, TARGETS[0])}.metadata.json"))
        with self.assertRaisesRegex(ValueError, "unexpected"):
            publish()
        self.assertTrue(release["draft"])
        self.assertEqual(self.api_writes, [])

    def test_incomplete_public_release_cannot_be_repaired_by_mutating_it(self):
        release, _ = self.publication_fixture(draft=False, partial=True)
        original = json.loads(json.dumps(release))
        with self.assertRaisesRegex(ValueError, "all six"):
            publish()
        self.assertEqual(release, original)
        self.assertEqual(self.uploaded_names, [])
        self.assertEqual(self.api_writes, [])

    def test_main_advancing_keeps_an_obsolete_nightly_private(self):
        nightly_tag = f"nightly-2026-10-01-{self.sha}"
        with patch.dict(os.environ, {"RELEASE_TAG": nightly_tag, "RELEASE_CHANNEL": "nightly"}):
            release, _ = self.publication_fixture()
            publish()
        self.assertTrue(release["draft"])
        self.assertEqual(self.api_writes, [])

    def test_tag_changing_after_upload_prevents_publication(self):
        release, _ = self.publication_fixture(partial=True)
        with patch("release.tag_sha", side_effect=[self.sha, self.sha, "f" * 40]):
            with self.assertRaisesRegex(ValueError, "tag changed before publication"):
                publish()
        self.assertTrue(release["draft"])
        self.assertEqual(self.api_writes, [])

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
