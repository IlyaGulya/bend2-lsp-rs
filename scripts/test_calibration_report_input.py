import contextlib
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
import zipfile

import calibration_report_input as transport

REPOSITORY = "IlyaGulya/bend2-lsp-rs"
RUN_ID = "40545135794"
RUN_ATTEMPT = "1"
SOURCE_REVISION = "12afbf96e8e5ed18462de42bdc1b265791673ff9"
WRAPPER_REVISION = "0123456789abcdef0123456789abcdef01234567"
SOURCE_WORKFLOW = ".github/workflows/performance-calibration.yml"
WRAPPER_WORKFLOW = ".github/workflows/performance-calibration-report-replay.yml"
RUNNER = "runner-a"
RUNNER_SHA = "b" * 64
SOURCE_ARTIFACT_ID = 1
SOURCE_ARTIFACT_DIGEST = "a" * 64
JOBS = (
    "discovery-01",
    "discovery-02",
    "discovery-03",
    "validation-01",
    "validation-02",
    "validation-03",
)


def sha(raw):
    return hashlib.sha256(raw).hexdigest()


def run_cli(arguments):
    stdout = io.StringIO()
    stderr = io.StringIO()
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        status = transport.main(arguments)
    return status, stdout.getvalue(), stderr.getvalue()


def dataset(job, index, count, runner=RUNNER):
    return {
        "format_version": 2,
        "job_id": job,
        "role": job.split("-", 1)[0],
        "source_revision": SOURCE_REVISION,
        "host": {"runner_identity": runner},
        "shard": {
            "method": transport.SHARD_METHOD,
            "count": count,
            "index": index,
            "logical_job_id": job,
            "physical_job_id": f"{job}-shard-{index}",
            "matrix_job": f"{job}-shard-{index}",
            "run_id": RUN_ID,
            "run_attempt": RUN_ATTEMPT,
            "physical_runner": runner,
        },
        "variants": {},
        "samples": [
            {
                "pair": 1,
                "variant": "a",
                "order": 0,
                "workloads": [],
                "benchmarks": {
                    "analysis": {
                        "raw_stdout": "raw/pair001a/analysis/stdout.jsonl",
                        "raw_stderr": "raw/pair001a/analysis/stderr.log",
                        "verified_callgrind_profiles": 1,
                        "execution": {"collection": "collection.json"},
                    }
                },
            }
        ],
    }


def write_tree(root, job, index, count, runner=RUNNER, data=None):
    root = Path(root)
    root.mkdir(parents=True)
    data = data if data is not None else dataset(job, index, count, runner)
    files = {
        "data.json": json.dumps(data, indent=2).encode(),
        "raw/pair001a/analysis/stdout.jsonl": b'{"function_name":"f","id":null}\n',
        "raw/pair001a/analysis/stderr.log": b"",
        "raw/pair001a/collection.json": json.dumps(
            {
                "parallelism": 4,
                "original_runner": "/tmp/iai-callgrind-runner",
                "original_runner_sha256": RUNNER_SHA,
                "functions": 1,
            }
        ).encode(),
        "collection-lifecycle.json": json.dumps(
            {
                "schema_version": 1,
                "spawned": True,
                "process_group": 42,
                "cleanup_complete": True,
                "exit_code": 0,
                "timed_out": False,
                "errors": [],
            }
        ).encode(),
        # Present in the full raw evidence but never in the compact closure.
        "raw/pair001a/analysis/iai/profile.bin": b"raw-callgrind-profile",
    }
    publication = {
        "schema_version": 1,
        "kind": "callgrind-evidence-snapshot",
        "status": "published",
        "source": str(root),
        "source_untouched": True,
        "collection_lifecycle": {},
        "completed_samples": ["raw/pair001a"],
        "incomplete_samples": [],
        "incomplete_namespace": "incomplete/raw",
        "partial_evidence_grants_admission": False,
    }
    rows = []
    for relative, raw in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(raw)
        rows.append(
            {
                "path": relative,
                "sha256": sha(raw),
                "size": len(raw),
                "category": "metadata" if relative in {
                    "data.json", "collection-lifecycle.json"
                } else "completed-raw",
            }
        )
    publication["files"] = sorted(rows, key=lambda row: row["path"])
    (root / "snapshot-publication.json").write_bytes(
        json.dumps(publication).encode()
    )
    return files


def build_all(root, shards=1):
    base = Path(root)
    output = base / "input"
    for job in JOBS:
        sealed = base / "sealed" / job
        write_tree(sealed, job, 0, shards)
        status, _, stderr = run_cli(
            [
                "build",
                "--sealed", str(sealed),
                "--output", str(output / job / "shard-0"),
                "--repository", REPOSITORY,
                "--run-id", RUN_ID,
                "--run-attempt", RUN_ATTEMPT,
                "--logical-job", job,
                "--shard-index", "0",
                "--shard-count", str(shards),
                "--expected-source-revision", SOURCE_REVISION,
                "--expected-runner", RUNNER,
                "--source-artifact-id",
                str(100 + JOBS.index(job)),
                "--source-artifact-digest",
                hashlib.sha256(job.encode()).hexdigest(),
                "--producer-workflow", SOURCE_WORKFLOW,
                "--producer-revision", WRAPPER_REVISION,
                "--producer-run-id", RUN_ID,
                "--producer-run-attempt", RUN_ATTEMPT,
            ]
        )
        if status != 0:
            raise AssertionError(stderr)
    return output


def verify(root, shards=1, source_format="sealed-tree", extra=None):
    args = [
        "verify",
        "--root", str(root),
        "--repository", REPOSITORY,
        "--run-id", RUN_ID,
        "--run-attempt", RUN_ATTEMPT,
        "--source-revision", SOURCE_REVISION,
        "--expected-producer-run-id", RUN_ID,
        "--expected-producer-run-attempt", RUN_ATTEMPT,
        "--expected-producer-workflow", SOURCE_WORKFLOW,
        "--expected-producer-revision", WRAPPER_REVISION,
        "--expected-discovery-jobs", "3",
        "--expected-validation-jobs", "3",
        "--expected-shards", str(shards),
        "--json-output", str(Path(root) / "verification.json"),
    ]
    if source_format is not None:
        args.extend(["--expected-source-format", source_format])
    if extra:
        args.extend(extra)
    return run_cli(args)


def reseal_descriptor(path):
    path = Path(path)
    value = json.loads(path.read_text())
    value.pop("input_sha256", None)
    value["input_sha256"] = transport.canonical_sha256(value)
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


class BuildTests(unittest.TestCase):
    def test_build_preserves_data_bytes_and_copies_only_closure(self):
        with tempfile.TemporaryDirectory() as temp:
            tree = Path(temp)
            files = write_tree(tree / "sealed", "discovery-01", 0, 4)
            output = tree / "input"
            status, _, stderr = run_cli(
                [
                    "build",
                    "--sealed", str(tree / "sealed"),
                    "--output", str(output),
                    "--repository", REPOSITORY,
                    "--run-id", RUN_ID,
                    "--run-attempt", RUN_ATTEMPT,
                    "--logical-job", "discovery-01",
                    "--shard-index", "0",
                    "--shard-count", "4",
                    "--expected-source-revision", SOURCE_REVISION,
                    "--expected-runner", RUNNER,
                    "--source-artifact-id", str(SOURCE_ARTIFACT_ID),
                    "--source-artifact-digest", SOURCE_ARTIFACT_DIGEST,
                    "--producer-workflow", SOURCE_WORKFLOW,
                    "--producer-revision", WRAPPER_REVISION,
                    "--producer-run-id", RUN_ID,
                    "--producer-run-attempt", RUN_ATTEMPT,
                ]
            )
            self.assertEqual(status, 0, stderr)
            self.assertEqual(
                (output / "data.json").read_bytes(), files["data.json"]
            )
            self.assertTrue(
                (output / "raw/pair001a/analysis/stdout.jsonl").is_file()
            )
            self.assertTrue(
                (output / "raw/pair001a/collection.json").is_file()
            )
            self.assertTrue((output / "collection-lifecycle.json").is_file())
            self.assertFalse(
                (output / "raw/pair001a/analysis/iai/profile.bin").exists()
            )
            descriptor = json.loads((output / "report-input.json").read_text())
            roles = {row["path"]: row["role"] for row in descriptor["files"]}
            self.assertEqual(roles["raw/pair001a/analysis/stdout.jsonl"], "raw_stdout")
            self.assertEqual(roles["raw/pair001a/analysis/stderr.log"], "raw_stderr")
            self.assertEqual(roles["raw/pair001a/collection.json"], "collection_receipt")
            self.assertEqual(roles["collection-lifecycle.json"], "collection_lifecycle")
            self.assertEqual(
                descriptor["source_evidence"]["publication_status"], "published"
            )

    def test_build_rejects_digest_contradiction(self):
        with tempfile.TemporaryDirectory() as temp:
            tree = Path(temp)
            write_tree(tree / "sealed", "discovery-01", 0, 4)
            target = tree / "sealed/raw/pair001a/analysis/stdout.jsonl"
            target.write_bytes(target.read_bytes() + b"tampered")
            status, _, stderr = run_cli(
                [
                    "build",
                    "--sealed", str(tree / "sealed"),
                    "--output", str(tree / "input"),
                    "--repository", REPOSITORY,
                    "--run-id", RUN_ID,
                    "--run-attempt", RUN_ATTEMPT,
                    "--logical-job", "discovery-01",
                    "--shard-index", "0",
                    "--shard-count", "4",
                    "--expected-source-revision", SOURCE_REVISION,
                    "--expected-runner", RUNNER,
                    "--source-artifact-id", str(SOURCE_ARTIFACT_ID),
                    "--source-artifact-digest", SOURCE_ARTIFACT_DIGEST,
                    "--producer-workflow", SOURCE_WORKFLOW,
                    "--producer-revision", WRAPPER_REVISION,
                    "--producer-run-id", RUN_ID,
                    "--producer-run-attempt", RUN_ATTEMPT,
                ]
            )
            self.assertEqual(status, 1)
            self.assertIn("digest contradicts", stderr)
            self.assertFalse((tree / "input").exists())

    def test_build_rejects_incomplete_publication(self):
        with tempfile.TemporaryDirectory() as temp:
            tree = Path(temp)
            write_tree(tree / "sealed", "discovery-01", 0, 4)
            publication_path = tree / "sealed/snapshot-publication.json"
            publication = json.loads(publication_path.read_text())
            publication["incomplete_samples"] = ["raw/pair001a"]
            publication_path.write_text(json.dumps(publication))
            status, _, stderr = run_cli(
                [
                    "build",
                    "--sealed", str(tree / "sealed"),
                    "--output", str(tree / "input"),
                    "--repository", REPOSITORY,
                    "--run-id", RUN_ID,
                    "--run-attempt", RUN_ATTEMPT,
                    "--logical-job", "discovery-01",
                    "--shard-index", "0",
                    "--shard-count", "4",
                    "--expected-source-revision", SOURCE_REVISION,
                    "--expected-runner", RUNNER,
                    "--source-artifact-id", str(SOURCE_ARTIFACT_ID),
                    "--source-artifact-digest", SOURCE_ARTIFACT_DIGEST,
                    "--producer-workflow", SOURCE_WORKFLOW,
                    "--producer-revision", WRAPPER_REVISION,
                    "--producer-run-id", RUN_ID,
                    "--producer-run-attempt", RUN_ATTEMPT,
                ]
            )
            self.assertEqual(status, 1)
            self.assertIn("incomplete samples", stderr)

    def test_build_rejects_escaping_dataset_path(self):
        with tempfile.TemporaryDirectory() as temp:
            tree = Path(temp)
            value = dataset("discovery-01", 0, 4)
            value["samples"][0]["benchmarks"]["analysis"]["raw_stdout"] = "../escape"
            write_tree(tree / "sealed", "discovery-01", 0, 4, data=value)
            status, _, stderr = run_cli(
                [
                    "build",
                    "--sealed", str(tree / "sealed"),
                    "--output", str(tree / "input"),
                    "--repository", REPOSITORY,
                    "--run-id", RUN_ID,
                    "--run-attempt", RUN_ATTEMPT,
                    "--logical-job", "discovery-01",
                    "--shard-index", "0",
                    "--shard-count", "4",
                    "--expected-source-revision", SOURCE_REVISION,
                    "--expected-runner", RUNNER,
                    "--source-artifact-id", str(SOURCE_ARTIFACT_ID),
                    "--source-artifact-digest", SOURCE_ARTIFACT_DIGEST,
                    "--producer-workflow", SOURCE_WORKFLOW,
                    "--producer-revision", WRAPPER_REVISION,
                    "--producer-run-id", RUN_ID,
                    "--producer-run-attempt", RUN_ATTEMPT,
                ]
            )
            self.assertEqual(status, 1)
            self.assertIn("must not contain empty", stderr)


class PlanTests(unittest.TestCase):
    def run_plan(self, run, artifacts, job="discovery-01"):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "run.json").write_text(json.dumps(run))
            (root / "artifacts.json").write_text(json.dumps(artifacts))
            status, stdout, stderr = run_cli(
                [
                    "plan",
                    "--run-json", str(root / "run.json"),
                    "--artifacts-json", str(root / "artifacts.json"),
                    "--repository", REPOSITORY,
                    "--run-id", RUN_ID,
                    "--run-attempt", RUN_ATTEMPT,
                    "--source-revision", SOURCE_REVISION,
                    "--logical-job", job,
                    "--shard-count", "4",
                    "--output", str(root / "plan.json"),
                ]
            )
            plan = None
            if (root / "plan.json").is_file():
                plan = json.loads((root / "plan.json").read_text())
            return status, stdout, stderr, plan

    def run_value(self, **updates):
        value = {
            "id": int(RUN_ID),
            "run_attempt": int(RUN_ATTEMPT),
            "head_sha": SOURCE_REVISION,
            "path": SOURCE_WORKFLOW,
            "status": "completed",
            "conclusion": "failure",
            "repository": {"full_name": REPOSITORY},
        }
        value.update(updates)
        return value

    def artifacts(self, job="discovery-01"):
        rows = [
            {
                "id": 99,
                "name": f"performance-calibration-report-{RUN_ID}-1",
                "size_in_bytes": 927,
                "digest": "sha256:" + "1" * 64,
                "expired": False,
            }
        ]
        for index in range(4):
            rows.append(
                {
                    "id": 100 + index,
                    "name": transport.expected_artifact_name(
                        RUN_ID, RUN_ATTEMPT, job, index
                    ),
                    "size_in_bytes": 1000 + index,
                    "digest": "sha256:" + (str(index) * 64),
                    "expired": False,
                }
            )
        return {"total_count": len(rows), "artifacts": rows}

    def test_plan_pins_only_the_four_original_shards(self):
        status, _, stderr, plan = self.run_plan(
            self.run_value(), self.artifacts()
        )
        self.assertEqual(status, 0, stderr)
        self.assertEqual(plan["source"]["source_revision"], SOURCE_REVISION)
        self.assertEqual(plan["source"]["workflow_conclusion"], "failure")
        self.assertEqual(len(plan["artifacts"]), 4)
        self.assertEqual(
            plan["artifacts"][3]["name"],
            f"performance-calibration-{RUN_ID}-1-discovery-01-shard-3",
        )

    def test_plan_rejects_wrong_attempt_or_source(self):
        for update in (
            {"run_attempt": 2},
            {"head_sha": "f" * 40},
            {"path": ".github/workflows/performance.yml"},
            {"status": "in_progress"},
        ):
            with self.subTest(update=update):
                status, _, stderr, _ = self.run_plan(
                    self.run_value(**update), self.artifacts()
                )
                self.assertEqual(status, 1, stderr)

    def test_plan_rejects_missing_extra_expired_or_bad_digest(self):
        missing = self.artifacts()
        missing["artifacts"].pop()
        missing["total_count"] = len(missing["artifacts"])
        status, _, _, _ = self.run_plan(self.run_value(), missing)
        self.assertEqual(status, 1)

        extra = self.artifacts()
        extra["artifacts"].append(
            {
                "id": 500,
                "name": f"performance-calibration-{RUN_ID}-1-discovery-01-shard-4",
                "size_in_bytes": 1,
                "digest": "sha256:" + "4" * 64,
                "expired": False,
            }
        )
        extra["total_count"] = len(extra["artifacts"])
        status, _, stderr, _ = self.run_plan(self.run_value(), extra)
        self.assertEqual(status, 1)
        self.assertIn("unexpected extra artifact", stderr)

        expired = self.artifacts()
        expired["artifacts"][1]["expired"] = True
        status, _, _, _ = self.run_plan(self.run_value(), expired)
        self.assertEqual(status, 1)

        bad = self.artifacts()
        bad["artifacts"][1]["digest"] = "sha256:nope"
        status, _, _, _ = self.run_plan(self.run_value(), bad)
        self.assertEqual(status, 1)


class ExportTests(unittest.TestCase):
    def make_zip(self, root, job="discovery-01"):
        sealed = Path(root) / "sealed"
        write_tree(sealed, job, 0, 1)
        archive = Path(root) / "source.zip"
        with zipfile.ZipFile(
            archive, "w", compression=zipfile.ZIP_STORED
        ) as handle:
            for path in sorted(sealed.rglob("*")):
                if path.is_file():
                    handle.write(path, path.relative_to(sealed).as_posix())
        raw = Path(root) / "raw" / transport.expected_artifact_name(
            RUN_ID, RUN_ATTEMPT, job, 0
        )
        raw.mkdir(parents=True)
        (raw / "artifact.zip").write_bytes(archive.read_bytes())
        plan = {
            "schema_version": 1,
            "kind": transport.PLAN_KIND,
            "source": {
                "repository": REPOSITORY,
                "run_id": RUN_ID,
                "run_attempt": RUN_ATTEMPT,
                "source_revision": SOURCE_REVISION,
                "workflow_path": SOURCE_WORKFLOW,
                "workflow_conclusion": "failure",
            },
            "logical_job": job,
            "role": "discovery",
            "shard_count": 1,
            "artifacts": [
                {
                    "name": transport.expected_artifact_name(
                        RUN_ID, RUN_ATTEMPT, job, 0
                    ),
                    "id": 100,
                    "size_in_bytes": archive.stat().st_size,
                    "digest": "sha256:" + hashlib.sha256(
                        archive.read_bytes()
                    ).hexdigest(),
                    "expired": False,
                }
            ],
        }
        (Path(root) / "plan.json").write_text(json.dumps(plan))
        return raw.parent, Path(root) / "plan.json", archive

    def export(self, root, plan, raw, output=None):
        return run_cli(
            [
                "export",
                "--plan", str(plan),
                "--raw-dir", str(raw),
                "--output", str(output or (Path(root) / "input")),
                "--producer-workflow", WRAPPER_WORKFLOW,
                "--producer-revision", WRAPPER_REVISION,
                "--producer-run-id", "7",
                "--producer-run-attempt", "1",
            ]
        )

    def test_export_reads_zip_bytes_without_extracting_profiles(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            raw, plan, _ = self.make_zip(root)
            status, stdout, stderr = self.export(root, plan, raw)
            self.assertEqual(status, 0, stderr)
            shard = root / "input" / "discovery-01" / "shard-0"
            self.assertTrue((shard / "data.json").is_file())
            self.assertFalse(
                (shard / "raw/pair001a/analysis/iai/profile.bin").exists()
            )
            descriptor = json.loads((shard / "report-input.json").read_text())
            self.assertEqual(
                descriptor["source_evidence"]["format"], "artifact-zip"
            )
            self.assertEqual(
                descriptor["source_evidence"]["artifact_id"], 100
            )

    def test_export_rejects_artifact_digest_mismatch(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            raw, plan, _ = self.make_zip(root)
            value = json.loads(plan.read_text())
            value["artifacts"][0]["digest"] = "sha256:" + "c" * 64
            plan.write_text(json.dumps(value))
            status, _, stderr = self.export(root, plan, raw)
            self.assertEqual(status, 1)
            self.assertIn("digest does not match", stderr)
            self.assertFalse((root / "input").exists())

    def test_export_rejects_crc_corruption(self):
        for marker in (b"discovery-01", b"raw-callgrind-profile"):
            with self.subTest(member_payload=marker), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                raw, plan, _ = self.make_zip(root)
                archive_path = next(raw.glob("*/artifact.zip"))
                raw_bytes = bytearray(archive_path.read_bytes())
                position = raw_bytes.find(marker)
                self.assertNotEqual(position, -1)
                raw_bytes[position] ^= 0x01
                archive_path.write_bytes(raw_bytes)
                # Match the delivered bytes: the member CRC, not its outer
                # digest, must reject both retained and omitted raw evidence.
                value = json.loads(plan.read_text())
                value["artifacts"][0]["size_in_bytes"] = len(raw_bytes)
                value["artifacts"][0]["digest"] = "sha256:" + hashlib.sha256(
                    bytes(raw_bytes)
                ).hexdigest()
                plan.write_text(json.dumps(value))
                status, _, stderr = self.export(root, plan, raw)
                self.assertEqual(status, 1)
                self.assertIn("CRC or decompression", stderr)
                self.assertFalse((root / "input" / "discovery-01" / "shard-0").exists())


class VerifyTests(unittest.TestCase):
    def test_verify_accepts_complete_compact_inputs(self):
        with tempfile.TemporaryDirectory() as temp:
            root = build_all(temp)
            status, stdout, stderr = verify(root)
            self.assertEqual(status, 0, stderr)
            self.assertIn("verified 6 compact shard inputs", stdout)
            summary = json.loads((root / "verification.json").read_text())
            self.assertEqual(summary["status"], "passed")

    def test_verify_rejects_tampered_file(self):
        with tempfile.TemporaryDirectory() as temp:
            root = build_all(temp)
            path = root / "discovery-01/shard-0/raw/pair001a/analysis/stdout.jsonl"
            path.write_bytes(path.read_bytes() + b"x")
            status, _, stderr = verify(root)
            self.assertEqual(status, 1)
            self.assertIn("digest changed", stderr)

    def test_verify_rejects_extra_file(self):
        with tempfile.TemporaryDirectory() as temp:
            root = build_all(temp)
            (root / "discovery-01/shard-0/rogue.json").write_text("{}")
            status, _, stderr = verify(root)
            self.assertEqual(status, 1)
            self.assertIn("extra", stderr)

    def test_verify_rejects_undescribed_data_json(self):
        with tempfile.TemporaryDirectory() as temp:
            root = build_all(temp)
            rogue = root / "rogue"
            rogue.mkdir()
            (rogue / "data.json").write_text(
                (root / "discovery-01/shard-0/data.json").read_text()
            )
            status, _, stderr = verify(root)
            self.assertEqual(status, 1)
            self.assertIn("undescribed data.json", stderr)

    def test_verify_rejects_missing_or_duplicate_shards(self):
        with tempfile.TemporaryDirectory() as temp:
            root = build_all(temp)
            (root / "validation-03/shard-0/report-input.json").unlink()
            status, _, stderr = verify(root)
            self.assertEqual(status, 1)
            self.assertIn("expected 6 compact shard inputs", stderr)

        with tempfile.TemporaryDirectory() as temp:
            root = build_all(temp)
            duplicate = root / "duplicate"
            duplicate.mkdir()
            source = root / "discovery-01/shard-0"
            for path in source.iterdir():
                if path.is_file():
                    (duplicate / path.name).write_bytes(path.read_bytes())
            status, _, stderr = verify(root)
            self.assertEqual(status, 1)

    def test_verify_rejects_source_or_producer_mismatch(self):
        with tempfile.TemporaryDirectory() as temp:
            root = build_all(temp)
            descriptor = root / "discovery-02/shard-0/report-input.json"
            value = json.loads(descriptor.read_text())
            value["dataset"]["source_revision"] = "f" * 40
            descriptor.write_text(json.dumps(value))
            reseal_descriptor(descriptor)
            status, _, stderr = verify(root)
            self.assertEqual(status, 1)
            self.assertIn("unexpected source revision", stderr)

        with tempfile.TemporaryDirectory() as temp:
            root = build_all(temp)
            descriptor = root / "discovery-02/shard-0/report-input.json"
            value = json.loads(descriptor.read_text())
            value["transport"]["producer"]["revision"] = "e" * 40
            descriptor.write_text(json.dumps(value))
            reseal_descriptor(descriptor)
            status, _, stderr = verify(root)
            self.assertEqual(status, 1)
            self.assertIn("unexpected producer revision", stderr)

    def test_verify_rejects_zip_digest_disagreement(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            raw, plan, archive = ExportTests().make_zip(root)
            status, _, stderr = ExportTests().export(root, plan, raw)
            self.assertEqual(status, 0, stderr)
            shard = root / "input" / "discovery-01" / "shard-0"
            descriptor_path = shard / "report-input.json"
            value = json.loads(descriptor_path.read_text())
            value["source_evidence"]["zip_sha256"] = "d" * 64
            descriptor_path.write_text(json.dumps(value))
            reseal_descriptor(descriptor_path)
            input_root = root / "input"
            status, _, stderr = run_cli(
                [
                    "verify",
                    "--root", str(input_root),
                    "--repository", REPOSITORY,
                    "--run-id", RUN_ID,
                    "--run-attempt", RUN_ATTEMPT,
                    "--source-revision", SOURCE_REVISION,
                    "--expected-producer-run-id", "7",
                    "--expected-producer-run-attempt", "1",
                    "--expected-producer-workflow", WRAPPER_WORKFLOW,
                    "--expected-producer-revision", WRAPPER_REVISION,
                    "--expected-discovery-jobs", "1",
                    "--expected-validation-jobs", "0",
                    "--expected-shards", "1",
                    "--expected-source-format", "artifact-zip",
                ]
            )
            self.assertEqual(status, 1)
            self.assertIn("digest and artifact digest disagree", stderr)


if __name__ == "__main__":
    unittest.main()
