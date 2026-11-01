#!/usr/bin/env python3
"""Transport-only publication and verification of compact calibration report inputs.

The authoritative validator remains ``bend2-perf calibration report``. This
helper never parses Callgrind output, never computes a statistic, never retries
failed evidence and never rewrites ``data.json``. It only proves that the bytes
the Rust validator needs are complete, unchanged and bound to the retained full
raw artifact.

Two producers are supported:

* ``build`` reads the sealed evidence tree that ``bend2-perf workflow seal``
  published in the collecting job.
* ``export`` reads the original artifact ZIP bytes without extracting them,
  verifies the GitHub artifact digest, and copies only the dependency closure.

The dependency closure is defined by the v2 dataset itself: ``data.json``, every
referenced ``raw_stdout``/``raw_stderr`` and every referenced pair collection
receipt. Raw Callgrind profiles stay exclusively in the full raw artifact.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sys
import tempfile
import zipfile

SCHEMA_VERSION = 1
INPUT_KIND = "calibration-report-input"
PLAN_KIND = "calibration-replay-source-plan"
VERIFICATION_KIND = "calibration-report-input-verification"
SHARD_METHOD = "sorted-function-unit-index-modulo-v1"
SOURCE_WORKFLOW = ".github/workflows/performance-calibration.yml"
VARIANTS = ("a", "b", "layout", "extra_work", "extra_alloc")
JOBS = (
    "discovery-01",
    "discovery-02",
    "discovery-03",
    "validation-01",
    "validation-02",
    "validation-03",
)
MANIFEST = "snapshot-publication.json"
DATA = "data.json"
LIFECYCLE = "collection-lifecycle.json"
PUBLICATION_COPY = "source-publication.json"
DESCRIPTOR = "report-input.json"
SHA256 = re.compile(r"[0-9a-f]{64}\Z")
ARTIFACT_DIGEST = re.compile(r"sha256:([0-9a-f]{64})\Z")
REVISION = re.compile(r"[0-9a-f]{40}\Z")
REPOSITORY = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+\Z")
SHA256_TAG = "sha256:"


class InputError(RuntimeError):
    """A transport invariant was violated; no partial input may be published."""


def require(condition, message):
    if not condition:
        raise InputError(message)


def sha256_bytes(value):
    return hashlib.sha256(value).hexdigest()


def canonical_bytes(value):
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode("ascii")


def canonical_sha256(value):
    return sha256_bytes(canonical_bytes(value))


def _reject_duplicate_keys(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            raise InputError(f"duplicate JSON key {key!r}")
        value[key] = item
    return value


def _reject_constant(name):
    raise InputError(f"non-finite JSON number {name!r} is not evidence")


def parse_json(raw, label):
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError as error:
        raise InputError(f"{label}: not UTF-8: {error}") from error
    try:
        return json.loads(
            text,
            object_pairs_hook=_reject_duplicate_keys,
            parse_constant=_reject_constant,
        )
    except json.JSONDecodeError as error:
        raise InputError(
            f"{label}: {error.msg} at byte {error.pos}"
        ) from error


def load_json(path, label=None):
    path = Path(path)
    try:
        raw = path.read_bytes()
    except OSError as error:
        raise InputError(f"{label or path}: {error}") from error
    return parse_json(raw, label or str(path))


def write_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(
        (
            json.dumps(value, indent=2, sort_keys=True, ensure_ascii=True) + "\n"
        ).encode("ascii")
    )


def require_digest(value, label):
    require(
        isinstance(value, str) and SHA256.fullmatch(value),
        f"{label} must be a lowercase SHA-256",
    )
    return value


def normalize_artifact_digest(value, label):
    """Accept the upload action's bare hex or the REST API's sha256: prefix."""
    if isinstance(value, str):
        if ARTIFACT_DIGEST.fullmatch(value):
            return value
        if SHA256.fullmatch(value):
            return SHA256_TAG + value
    raise InputError(f"{label} must be a SHA-256 digest")


def require_revision(value, label):
    require(
        isinstance(value, str) and REVISION.fullmatch(value),
        f"{label} must be an exact lowercase 40-character revision",
    )
    return value


def require_text(value, label):
    require(
        isinstance(value, str) and value.strip() and value == value.strip(),
        f"{label} must be a nonempty string without surrounding whitespace",
    )
    return value


def require_integer(value, label, minimum=None):
    require(
        isinstance(value, int) and not isinstance(value, bool),
        f"{label} must be an integer",
    )
    if minimum is not None:
        require(value >= minimum, f"{label} must be at least {minimum}")
    return value


def require_repository(value, label="repository"):
    require(
        isinstance(value, str) and REPOSITORY.fullmatch(value),
        f"{label} must be an owner/name repository slug",
    )
    return value


def role_for_job(job):
    require(job in JOBS, f"unsupported logical job {job!r}")
    return job.split("-", 1)[0]


def expected_artifact_name(run_id, run_attempt, job, index):
    return f"performance-calibration-{run_id}-{run_attempt}-{job}-shard-{index}"


def safe_relative(value, label):
    require(isinstance(value, str) and value, f"{label} must be a relative path")
    require("\\" not in value, f"{label} must use '/' separators: {value!r}")
    path = PurePosixPath(value)
    require(not path.is_absolute(), f"{label} must not be absolute: {value!r}")
    require(
        all(part not in ("", ".", "..") for part in path.parts),
        f"{label} must not contain empty, '.' or '..' components: {value!r}",
    )
    require(str(path) == value, f"{label} must be normalized: {value!r}")
    return path


def checked_path(root, relative, label):
    """Resolve an analyzed relative path below root without following links."""
    path = safe_relative(relative, label)
    candidate = Path(root)
    for part in path.parts:
        candidate = candidate / part
        if candidate.is_symlink():
            raise InputError(f"{label} must not traverse a symlink: {relative!r}")
    try:
        resolved = candidate.resolve(strict=True)
    except OSError as error:
        raise InputError(f"{label} is missing: {relative!r}: {error}") from error
    require(
        resolved == candidate and resolved.is_file(),
        f"{label} must be an ordinary file: {relative!r}",
    )
    return candidate


def read_regular(path, label):
    require(not path.is_symlink(), f"{label} must not be a symlink: {path}")
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        require(
            os.fstat(descriptor).st_mode & 0o170000 == 0o100000,
            f"{label} must be an ordinary file: {path}",
        )
        with os.fdopen(descriptor, "rb", closefd=False) as handle:
            return handle.read()
    finally:
        os.close(descriptor)


def hash_file(path, label):
    require(not path.is_symlink(), f"{label} must not be a symlink: {path}")
    hasher = hashlib.sha256()
    size = 0
    descriptor = os.open(path, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
    try:
        require(
            os.fstat(descriptor).st_mode & 0o170000 == 0o100000,
            f"{label} must be an ordinary file: {path}",
        )
        with os.fdopen(descriptor, "rb", closefd=False) as handle:
            while True:
                chunk = handle.read(1 << 20)
                if not chunk:
                    break
                hasher.update(chunk)
                size += len(chunk)
    finally:
        os.close(descriptor)
    return hasher.hexdigest(), size


class DirectorySource:
    """Sealed evidence tree published by ``bend2-perf workflow seal``."""

    format = "sealed-tree"

    def __init__(self, root):
        self.root = Path(root).resolve()
        require(self.root.is_dir(), f"sealed evidence root is missing: {self.root}")
        require(
            not self.root.is_symlink(),
            f"sealed evidence root must not be a symlink: {self.root}",
        )

    def path(self, relative):
        return checked_path(self.root, relative, "sealed evidence")

    def read(self, relative):
        return read_regular(self.path(relative), f"sealed evidence {relative}")

    def copy(self, relative, destination):
        source = self.path(relative)
        hasher = hashlib.sha256()
        size = 0
        destination.parent.mkdir(parents=True, exist_ok=True)
        require(not destination.exists(), f"duplicate output path {destination}")
        descriptor = os.open(source, os.O_RDONLY | getattr(os, "O_NOFOLLOW", 0))
        try:
            require(
                os.fstat(descriptor).st_mode & 0o170000 == 0o100000,
                f"sealed evidence must be an ordinary file: {source}",
            )
            with os.fdopen(descriptor, "rb", closefd=False) as reader, open(
                destination, "xb"
            ) as writer:
                while True:
                    chunk = reader.read(1 << 20)
                    if not chunk:
                        break
                    hasher.update(chunk)
                    size += len(chunk)
                    writer.write(chunk)
                writer.flush()
                os.fsync(writer.fileno())
        finally:
            os.close(descriptor)
        return hasher.hexdigest(), size


class ZipSource:
    """Original uploaded artifact ZIP read in place, never bulk-extracted."""

    format = "artifact-zip"

    def __init__(self, path, label):
        self.path = Path(path)
        self.label = label
        require(
            not self.path.is_symlink() and self.path.is_file(),
            f"{label} must be an ordinary ZIP file: {self.path}",
        )
        try:
            self.archive = zipfile.ZipFile(self.path)
        except (OSError, zipfile.BadZipFile) as error:
            raise InputError(f"{label} is not a readable ZIP: {error}") from error
        self.members = {}
        for info in self.archive.infolist():
            if info.is_dir():
                continue
            safe_relative(info.filename, f"{label} member name")
            require(
                info.filename not in self.members,
                f"{label} contains duplicate member {info.filename!r}",
            )
            self.members[info.filename] = info

    def verify_crc(self):
        try:
            corrupt = self.archive.testzip()
        except (zipfile.BadZipFile, OSError, RuntimeError) as error:
            raise InputError(
                f"{self.label} failed CRC or decompression: {error}"
            ) from error
        require(
            corrupt is None,
            f"{self.label} member {corrupt!r} failed CRC or decompression",
        )

    def close(self):
        self.archive.close()

    def member(self, relative):
        path = safe_relative(relative, f"{self.label} dependency")
        info = self.members.get(str(path))
        require(info is not None, f"{self.label} is missing {relative!r}")
        return info

    def read(self, relative):
        info = self.member(relative)
        try:
            return self.archive.read(info)
        except (zipfile.BadZipFile, OSError, RuntimeError) as error:
            raise InputError(
                f"{self.label} member {relative!r} failed CRC or decompression: {error}"
            ) from error

    def copy(self, relative, destination):
        info = self.member(relative)
        hasher = hashlib.sha256()
        size = 0
        destination.parent.mkdir(parents=True, exist_ok=True)
        require(not destination.exists(), f"duplicate output path {destination}")
        try:
            with self.archive.open(info) as reader, open(destination, "xb") as writer:
                while True:
                    chunk = reader.read(1 << 20)
                    if not chunk:
                        break
                    hasher.update(chunk)
                    size += len(chunk)
                    writer.write(chunk)
                writer.flush()
                os.fsync(writer.fileno())
            if size != info.file_size:
                raise InputError(
                    f"{self.label} member {relative!r} declared {info.file_size} "
                    f"bytes but produced {size}"
                )
        except (zipfile.BadZipFile, OSError, RuntimeError) as error:
            raise InputError(
                f"{self.label} member {relative!r} failed CRC or decompression: {error}"
            ) from error
        return hasher.hexdigest(), size


def validate_dataset_identity(
    data,
    *,
    job,
    role,
    index,
    count,
    run_id,
    run_attempt,
    source_revision,
    label,
):
    require(isinstance(data, dict), f"{label} must be a JSON object")
    require(
        data.get("format_version") == 2,
        f"{label}: compact report inputs require sharded format_version 2",
    )
    require(data.get("job_id") == job, f"{label}: job_id must be {job!r}")
    require(data.get("role") == role, f"{label}: role must be {role!r}")
    require(
        data.get("source_revision") == source_revision,
        f"{label}: source_revision must be {source_revision}",
    )
    host = data.get("host")
    require(isinstance(host, dict), f"{label}: host metadata must be an object")
    shard = data.get("shard")
    require(isinstance(shard, dict), f"{label}: shard provenance must be an object")
    require(shard.get("method") == SHARD_METHOD, f"{label}: unexpected shard method")
    require(
        require_integer(shard.get("count"), f"{label}: shard count", 1) == count,
        f"{label}: shard count must be {count}",
    )
    require(
        require_integer(shard.get("index"), f"{label}: shard index", 0) == index,
        f"{label}: shard index must be {index}",
    )
    physical = f"{job}-shard-{index}"
    require(
        shard.get("logical_job_id") == job,
        f"{label}: shard logical job must equal job_id",
    )
    require(
        shard.get("physical_job_id") == physical,
        f"{label}: shard physical job must be {physical!r}",
    )
    require(
        shard.get("matrix_job") == physical,
        f"{label}: shard matrix job must be {physical!r}",
    )
    require(
        shard.get("run_id") == run_id,
        f"{label}: shard run_id must be {run_id!r}",
    )
    require(
        shard.get("run_attempt") == run_attempt,
        f"{label}: shard run_attempt must be {run_attempt!r}",
    )
    runner = require_text(shard.get("physical_runner"), f"{label}: physical runner")
    require(
        shard.get("physical_runner") == host.get("runner_identity"),
        f"{label}: physical runner must match retained host identity",
    )
    samples = data.get("samples")
    require(
        isinstance(samples, list) and samples,
        f"{label}: samples must be a nonempty list",
    )
    return runner


def dataset_closure(data, *, label):
    """Return the exact file closure of a validated v2 dataset.

    Path layout and provenance questions stay structural: pair directories must
    match pair/variant, every raw evidence file must be distinct, and every pair
    declares one collection receipt. Statistical checks remain in Rust.
    """
    entries = {}
    collections = {}
    seen_raw = set()
    samples = data["samples"]
    for position, sample in enumerate(samples):
        sample_label = f"{label}: samples[{position}]"
        require(isinstance(sample, dict), f"{sample_label} must be an object")
        number = require_integer(
            sample.get("pair"), f"{sample_label}.pair", 1
        )
        variant = sample.get("variant")
        require(
            variant in VARIANTS, f"{sample_label}.variant is unsupported"
        )
        require_integer(sample.get("order"), f"{sample_label}.order", 0)
        benchmarks = sample.get("benchmarks")
        require(
            isinstance(benchmarks, dict) and benchmarks,
            f"{sample_label}.benchmarks must be a nonempty object",
        )
        pair_directory = f"pair{number:03d}{variant.replace('_', '')}"
        for benchmark, evidence in sorted(benchmarks.items()):
            evidence_label = f"{sample_label}.benchmarks[{benchmark!r}]"
            require_text(benchmark, f"{evidence_label} name")
            require(
                isinstance(evidence, dict),
                f"{evidence_label} must be an object",
            )
            stdout = safe_relative(
                evidence.get("raw_stdout"), f"{evidence_label}.raw_stdout"
            )
            stderr = safe_relative(
                evidence.get("raw_stderr"), f"{evidence_label}.raw_stderr"
            )
            require(
                len(stdout.parts) >= 3 and len(stderr.parts) >= 3,
                f"{evidence_label}: raw evidence must live below raw/PAIR/BENCHMARK",
            )
            require(
                stdout.parent.parent == stderr.parent.parent,
                f"{evidence_label}: stdout and stderr must share one pair directory",
            )
            pair = stdout.parent.parent
            require(
                pair.parts[0] == "raw" and len(pair.parts) == 2,
                f"{evidence_label}: raw evidence must be inside raw/<pair>",
            )
            require(
                pair.name == pair_directory,
                f"{evidence_label}: evidence directory {pair.name!r} does not "
                f"match pair/variant {pair_directory!r}",
            )
            execution = evidence.get("execution")
            require(
                isinstance(execution, dict),
                f"{evidence_label}.execution must be an object",
            )
            collection = safe_relative(
                execution.get("collection"),
                f"{evidence_label}.execution.collection",
            )
            receipt = str(pair / collection)
            if str(pair) in collections:
                require(
                    collections[str(pair)] == receipt,
                    f"{evidence_label}: pair declares inconsistent collection receipts",
                )
            else:
                collections[str(pair)] = receipt
            for relative, role in (
                (stdout, "raw_stdout"),
                (stderr, "raw_stderr"),
            ):
                require(
                    str(relative) not in seen_raw,
                    f"{evidence_label}: raw evidence is reused across executions",
                )
                seen_raw.add(str(relative))
                entries[str(relative)] = role
            entries[receipt] = "collection_receipt"
    require(collections, f"{label}: dataset has no dependency closure")
    return entries, collections


def publication_index(publication, label):
    require(
        isinstance(publication, dict),
        f"{label} must be a JSON object",
    )
    require(
        publication.get("schema_version") == 1,
        f"{label}: unexpected schema_version",
    )
    require(
        publication.get("kind") == "callgrind-evidence-snapshot",
        f"{label}: unexpected kind",
    )
    require(
        publication.get("status") == "published",
        f"{label}: only a published snapshot may supply a report input",
    )
    require(
        publication.get("partial_evidence_grants_admission") is False,
        f"{label}: partial evidence must never grant admission",
    )
    require(
        publication.get("source_untouched") is True,
        f"{label}: source must be declared untouched",
    )
    require(
        publication.get("incomplete_samples") == [],
        f"{label}: incomplete samples forbid a compact report input",
    )
    require(
        isinstance(publication.get("completed_samples"), list),
        f"{label}: completed_samples must be a list",
    )
    rows = publication.get("files")
    require(isinstance(rows, list) and rows, f"{label}: files must be nonempty")
    index = {}
    for position, row in enumerate(rows):
        row_label = f"{label}: files[{position}]"
        require(isinstance(row, dict), f"{row_label} must be an object")
        relative = safe_relative(row.get("path"), f"{row_label}.path")
        name = str(relative)
        require(name not in index, f"{row_label}: duplicate path {name!r}")
        require_digest(row.get("sha256"), f"{row_label}.sha256")
        require_integer(row.get("size"), f"{row_label}.size", 0)
        category = require_text(row.get("category"), f"{row_label}.category")
        index[name] = {
            "sha256": row["sha256"],
            "size": row["size"],
            "category": category,
        }
    return index


def verify_publication_entry(index, relative, sha256, size, categories, label):
    entry = index.get(relative)
    require(entry is not None, f"{label}: {relative!r} is not published")
    require(
        entry["sha256"] == sha256,
        f"{label}: {relative!r} digest contradicts the publication manifest",
    )
    require(
        entry["size"] == size,
        f"{label}: {relative!r} size contradicts the publication manifest",
    )
    require(
        entry["category"] in categories,
        f"{label}: {relative!r} has unexpected publication category "
        f"{entry['category']!r}",
    )


def copy_closure(
    source,
    stage,
    closure,
    index,
    label,
    files,
):
    for relative in sorted(closure):
        role = closure[relative]
        category = "completed-raw"
        destination = Path(stage) / relative
        sha256, size = source.copy(relative, destination)
        verify_publication_entry(
            index, relative, sha256, size, (category,), label
        )
        files.append(
            {"path": relative, "role": role, "sha256": sha256, "bytes": size}
        )
    return files


def collection_receipts(source, collections, label):
    runners = set()
    for relative in sorted(set(collections.values())):
        raw = source.read(relative)
        receipt = parse_json(raw, f"{label}: {relative}")
        require(
            isinstance(receipt, dict),
            f"{label}: {relative} must be a JSON object",
        )
        runner = require_digest(
            receipt.get("original_runner_sha256"),
            f"{label}: {relative}.original_runner_sha256",
        )
        runners.add(runner)
    require(
        len(runners) == 1,
        f"{label}: pair receipts must pin exactly one original runner",
    )
    return next(iter(runners))


def publish(
    source,
    *,
    output,
    repository,
    run_id,
    run_attempt,
    job,
    index,
    count,
    source_revision,
    expected_runner=None,
    producer,
    source_evidence,
):
    role = role_for_job(job)
    output = Path(output)
    require(
        not output.exists(),
        f"compact report input destination already exists: {output}",
    )
    output.parent.mkdir(parents=True, exist_ok=True)
    stage = Path(
        tempfile.mkdtemp(
            prefix=".calibration-report-input-",
            dir=output.parent,
        )
    )
    try:
        label = f"{job} shard {index}"
        data_raw = source.read(DATA)
        data = parse_json(data_raw, f"{label}: {DATA}")
        data_runner = validate_dataset_identity(
            data,
            job=job,
            role=role,
            index=index,
            count=count,
            run_id=run_id,
            run_attempt=run_attempt,
            source_revision=source_revision,
            label=label,
        )
        if expected_runner is not None:
            require(
                data_runner == expected_runner,
                f"{label}: physical runner must be {expected_runner!r}",
            )
        closure, collections = dataset_closure(data, label=label)
        publication_raw = source.read(MANIFEST)
        publication = parse_json(publication_raw, f"{label}: {MANIFEST}")
        index_entries = publication_index(publication, f"{label}: {MANIFEST}")
        data_sha = sha256_bytes(data_raw)
        data_size = len(data_raw)
        verify_publication_entry(
            index_entries, DATA, data_sha, data_size, ("metadata",), label
        )

        files = []
        copy_closure(
            source,
            stage,
            closure,
            index_entries,
            label,
            files,
        )
        lifecycle_sha, lifecycle_size = source.copy(
            LIFECYCLE, stage / LIFECYCLE
        )
        verify_publication_entry(
            index_entries,
            LIFECYCLE,
            lifecycle_sha,
            lifecycle_size,
            ("metadata",),
            label,
        )
        files.append(
            {
                "path": LIFECYCLE,
                "role": "collection_lifecycle",
                "sha256": lifecycle_sha,
                "bytes": lifecycle_size,
            }
        )
        runner = collection_receipts(source, collections, label)
        (stage / DATA).write_bytes(data_raw)
        (stage / PUBLICATION_COPY).write_bytes(publication_raw)

        files.sort(key=lambda row: row["path"])
        descriptor = {
            "schema_version": SCHEMA_VERSION,
            "kind": INPUT_KIND,
            "transport": {
                "repository": repository,
                "run_id": run_id,
                "run_attempt": run_attempt,
                "producer": producer,
            },
            "dataset": {
                "job_id": job,
                "role": role,
                "source_revision": source_revision,
                "physical_job_id": f"{job}-shard-{index}",
                "shard": {
                    "method": SHARD_METHOD,
                    "index": index,
                    "count": count,
                },
                "runner_sha256": runner,
                "host_runner_identity": data_runner,
                "data_sha256": data_sha,
                "data_bytes": data_size,
                "samples": len(data["samples"]),
                "pairs": len(collections),
            },
            "source_evidence": {
                **source_evidence,
                "publication_manifest_path": PUBLICATION_COPY,
                "publication_manifest_sha256": sha256_bytes(publication_raw),
                "publication_source": publication.get("source"),
                "publication_status": publication["status"],
                "publication_file_count": len(index_entries),
                "completed_samples": len(publication["completed_samples"]),
                "incomplete_samples": 0,
            },
            "files": files,
        }
        descriptor["input_sha256"] = canonical_sha256(
            {key: value for key, value in descriptor.items() if key != "input_sha256"}
        )
        write_json(stage / DESCRIPTOR, descriptor)
        os.rename(stage, output)
        return descriptor
    except BaseException:
        shutil.rmtree(stage, ignore_errors=True)
        raise


def producer_block(args):
    producer = {
        "workflow_path": require_text(
            args.producer_workflow, "producer workflow path"
        ),
        "revision": require_revision(
            args.producer_revision, "producer revision"
        ),
        "run_id": require_text(args.producer_run_id, "producer run id"),
        "run_attempt": require_text(
            args.producer_run_attempt, "producer run attempt"
        ),
    }
    require(
        producer["workflow_path"].startswith(".github/workflows/"),
        "producer workflow path must be a workflow file",
    )
    return producer


def command_build(args):
    run_id = require_text(args.run_id, "run id")
    run_attempt = require_text(args.run_attempt, "run attempt")
    count = require_integer(args.shard_count, "shard count", 1)
    index = require_integer(args.shard_index, "shard index", 0)
    require(index < count, "shard index must be less than shard count")
    repository = require_repository(args.repository)
    source_revision = require_revision(
        args.expected_source_revision, "expected source revision"
    )
    artifact_name = expected_artifact_name(
        run_id, run_attempt, args.logical_job, index
    )
    artifact_id = require_integer(
        args.source_artifact_id, "source artifact id", 1
    )
    artifact_digest = normalize_artifact_digest(
        args.source_artifact_digest, "source artifact digest"
    )
    descriptor = publish(
        DirectorySource(args.sealed),
        output=args.output,
        repository=repository,
        run_id=run_id,
        run_attempt=run_attempt,
        job=args.logical_job,
        index=index,
        count=count,
        source_revision=source_revision,
        expected_runner=require_text(
            args.expected_runner, "expected runner identity"
        ),
        producer=producer_block(args),
        source_evidence={
            "format": "sealed-tree",
            "artifact_name": artifact_name,
            "artifact_id": artifact_id,
            "artifact_digest": artifact_digest,
        },
    )
    print(
        f"published {artifact_name}: {len(descriptor['files'])} files, "
        f"input {descriptor['input_sha256']}"
    )
    return 0


def flatten_artifacts(value, label):
    pages = value if isinstance(value, list) else [value]
    require(pages, f"{label} must contain at least one page")
    artifacts = []
    totals = set()
    for position, page in enumerate(pages):
        require(
            isinstance(page, dict),
            f"{label} page {position} must be an object",
        )
        total = require_integer(
            page.get("total_count"), f"{label} page {position}.total_count", 0
        )
        totals.add(total)
        rows = page.get("artifacts")
        require(
            isinstance(rows, list),
            f"{label} page {position}.artifacts must be a list",
        )
        artifacts.extend(rows)
    require(
        len(totals) == 1,
        f"{label} pages disagree on total_count",
    )
    total = next(iter(totals))
    require(
        total == len(artifacts),
        f"{label}: received {len(artifacts)} of {total} artifacts; "
        "the listing is truncated",
    )
    return artifacts


def command_plan(args):
    repository = require_repository(args.repository)
    run_id = require_text(args.run_id, "run id")
    run_attempt = require_text(args.run_attempt, "run attempt")
    source_revision = require_revision(
        args.source_revision, "source revision"
    )
    count = require_integer(args.shard_count, "shard count", 1)
    require(
        count == 4,
        "the original control run used exactly four physical shards",
    )
    role = role_for_job(args.logical_job)
    run = load_json(args.run_json, "source run")
    require(isinstance(run, dict), "source run must be an object")
    require(
        str(run.get("id")) == run_id,
        f"source run id must be {run_id}, found {run.get('id')!r}",
    )
    require(
        str(run.get("run_attempt")) == run_attempt,
        f"source run attempt must be {run_attempt}, "
        f"found {run.get('run_attempt')!r}",
    )
    require(
        run.get("head_sha") == source_revision,
        f"source run head_sha must be {source_revision}, "
        f"found {run.get('head_sha')!r}",
    )
    require(
        run.get("path") == SOURCE_WORKFLOW,
        "source run must be the performance-calibration workflow",
    )
    require(
        run.get("status") == "completed",
        "source run must be completed before artifacts are exported",
    )
    repository_identity = run.get("repository")
    require(
        isinstance(repository_identity, dict)
        and repository_identity.get("full_name") == repository,
        f"source run repository must be {repository}",
    )

    listed = flatten_artifacts(
        load_json(args.artifacts_json, "source run artifacts"),
        "source run artifacts",
    )
    expected = {}
    for index in range(count):
        name = expected_artifact_name(
            run_id, run_attempt, args.logical_job, index
        )
        expected[name] = index
    prefix = (
        f"performance-calibration-{run_id}-{run_attempt}-"
        f"{args.logical_job}-shard-"
    )
    selected = {}
    seen_ids = set()
    seen_digests = set()
    for position, artifact in enumerate(listed):
        label = f"source run artifacts[{position}]"
        require(isinstance(artifact, dict), f"{label} must be an object")
        name = artifact.get("name")
        require(isinstance(name, str), f"{label}.name must be a string")
        if not name.startswith(prefix):
            continue
        require(
            name in expected,
            f"unexpected extra artifact for {args.logical_job}: {name!r}",
        )
        require(name not in selected, f"duplicate artifact name {name!r}")
        require(
            artifact.get("expired") is False,
            f"artifact {name!r} is expired",
        )
        artifact_id = require_integer(
            artifact.get("id"), f"{label}.id", 1
        )
        size = require_integer(
            artifact.get("size_in_bytes"), f"{label}.size_in_bytes", 1
        )
        digest = normalize_artifact_digest(
            artifact.get("digest"), f"artifact {name!r} digest"
        )
        require(
            artifact_id not in seen_ids,
            f"duplicate source artifact id {artifact_id}",
        )
        require(
            digest not in seen_digests,
            f"duplicate source artifact digest {digest}",
        )
        seen_ids.add(artifact_id)
        seen_digests.add(digest)
        selected[name] = {
            "name": name,
            "id": artifact_id,
            "size_in_bytes": size,
            "digest": digest,
            "expired": False,
        }
    for name in expected:
        require(name in selected, f"missing source artifact {name!r}")

    plan = {
        "schema_version": SCHEMA_VERSION,
        "kind": PLAN_KIND,
        "source": {
            "repository": repository,
            "run_id": run_id,
            "run_attempt": run_attempt,
            "source_revision": source_revision,
            "workflow_path": run["path"],
            "workflow_conclusion": run.get("conclusion"),
        },
        "logical_job": args.logical_job,
        "role": role,
        "shard_count": count,
        "artifacts": [selected[name] for name in sorted(
            selected, key=lambda value: expected[value]
        )],
    }
    write_json(args.output, plan)
    print(
        f"planned {len(selected)} original artifacts for {args.logical_job} "
        f"from run {run_id} attempt {run_attempt}"
    )
    return 0


def locate_downloaded_archives(raw_root, expected):
    raw_root = Path(raw_root)
    require(raw_root.is_dir(), f"artifact download root is missing: {raw_root}")
    entries = list(os.scandir(raw_root))
    names = set()
    for entry in entries:
        require(
            entry.is_dir(follow_symlinks=False),
            f"unexpected download entry {entry.name!r}; "
            "each matched artifact must have its own directory",
        )
        names.add(entry.name)
    require(
        names == set(expected),
        "downloaded artifact directories do not match the pinned plan: "
        f"expected {sorted(expected)}, found {sorted(names)}",
    )
    archives = {}
    for name in expected:
        directory = raw_root / name
        children = list(os.scandir(directory))
        require(
            len(children) == 1,
            f"downloaded artifact {name!r} must contain exactly one ZIP byte stream",
        )
        child = children[0]
        require(
            child.is_file(follow_symlinks=False),
            f"downloaded artifact {name!r} contains an unexpected entry "
            f"{child.name!r}",
        )
        archives[name] = Path(child.path)
    return archives


def command_export(args):
    plan = load_json(args.plan, "replay plan")
    require(isinstance(plan, dict), "replay plan must be an object")
    require(plan.get("schema_version") == SCHEMA_VERSION, "unexpected plan schema")
    require(plan.get("kind") == PLAN_KIND, "unexpected plan kind")
    source = plan.get("source")
    require(isinstance(source, dict), "plan source must be an object")
    require(
        source.get("workflow_path") == SOURCE_WORKFLOW,
        "plan source must be the performance-calibration workflow",
    )
    repository = require_repository(source.get("repository"))
    run_id = require_text(source.get("run_id"), "plan source run id")
    run_attempt = require_text(source.get("run_attempt"), "plan source run attempt")
    source_revision = require_revision(
        source.get("source_revision"), "plan source revision"
    )
    job = require_text(plan.get("logical_job"), "plan logical job")
    role = role_for_job(job)
    require(plan.get("role") == role, "plan role must match the logical job")
    count = require_integer(plan.get("shard_count"), "plan shard count", 1)
    rows = plan.get("artifacts")
    require(isinstance(rows, list), "plan artifacts must be a list")
    require(len(rows) == count, "plan must contain every physical shard")
    expected = {}
    for position, row in enumerate(rows):
        label = f"plan artifacts[{position}]"
        require(isinstance(row, dict), f"{label} must be an object")
        name = require_text(row.get("name"), f"{label}.name")
        require(name not in expected, f"duplicate plan artifact {name!r}")
        expected[name] = row
    for index in range(count):
        name = expected_artifact_name(run_id, run_attempt, job, index)
        require(name in expected, f"plan is missing {name!r}")
    require(
        len(expected) == count,
        "plan must not contain extra artifacts",
    )

    archives = locate_downloaded_archives(args.raw_dir, expected)
    producer = producer_block(args)
    for index, name in enumerate(
        sorted(expected, key=lambda value: int(value.rsplit("-", 1)[1]))
    ):
        row = expected[name]
        artifact_id = require_integer(row.get("id"), f"{name}.id", 1)
        size = require_integer(row.get("size_in_bytes"), f"{name}.size_in_bytes", 1)
        digest = normalize_artifact_digest(
            row.get("digest"), f"{name} digest"
        )
        archive = archives[name]
        archive_sha, archive_size = hash_file(archive, f"{name} ZIP")
        require(
            archive_size == size,
            f"{name}: downloaded ZIP has {archive_size} bytes, expected {size}",
        )
        require(
            SHA256_TAG + archive_sha == digest,
            f"{name}: downloaded ZIP digest does not match the pinned artifact",
        )
        zip_source = ZipSource(archive, f"{name} ZIP")
        try:
            zip_source.verify_crc()
            names = sorted(zip_source.members)
            data_members = [
                member
                for member in names
                if PurePosixPath(member).name == DATA
            ]
            require(
                data_members == [DATA],
                f"{name}: ZIP must contain exactly one root data.json, "
                f"found {data_members}",
            )
            publish(
                zip_source,
                output=Path(args.output) / job / f"shard-{index}",
                repository=repository,
                run_id=run_id,
                run_attempt=run_attempt,
                job=job,
                index=index,
                count=count,
                source_revision=source_revision,
                producer=producer,
                source_evidence={
                    "format": "artifact-zip",
                    "artifact_name": name,
                    "artifact_id": artifact_id,
                    "artifact_size": size,
                    "artifact_digest": digest,
                    "zip_sha256": archive_sha,
                    "zip_bytes": archive_size,
                },
            )
        finally:
            zip_source.close()
    return 0


def descriptor_paths(directory):
    directory = Path(directory)
    paths = []
    for current, directories, files in os.walk(directory, followlinks=False):
        current = Path(current)
        for name in list(directories):
            if (current / name).is_symlink():
                raise InputError(
                    f"compact report input contains a symlink: {current / name}"
                )
        if DESCRIPTOR in files:
            candidate = current / DESCRIPTOR
            if candidate.is_symlink():
                raise InputError(
                    f"compact report input contains a symlink: {candidate}"
                )
            paths.append(candidate)
    return sorted(paths)


def regular_files(directory):
    directory = Path(directory)
    files = {}
    for current, directories, names in os.walk(directory, followlinks=False):
        current = Path(current)
        for name in list(directories):
            if (current / name).is_symlink():
                raise InputError(
                    f"compact report input contains a symlink: {current / name}"
                )
        for name in names:
            path = current / name
            if path.is_symlink():
                raise InputError(f"compact report input contains a symlink: {path}")
            relative = path.relative_to(directory).as_posix()
            files[relative] = path
    return files


def verify_descriptor(
    descriptor_path,
    *,
    args,
    observations,
):
    label = str(descriptor_path)
    directory = descriptor_path.parent
    descriptor = load_json(descriptor_path, label)
    require(isinstance(descriptor, dict), f"{label} must be an object")
    require(
        descriptor.get("schema_version") == SCHEMA_VERSION,
        f"{label}: unexpected schema_version",
    )
    require(
        descriptor.get("kind") == INPUT_KIND,
        f"{label}: unexpected kind",
    )
    recorded = require_digest(
        descriptor.get("input_sha256"), f"{label}.input_sha256"
    )
    require(
        canonical_sha256(
            {key: value for key, value in descriptor.items() if key != "input_sha256"}
        )
        == recorded,
        f"{label}: input_sha256 does not cover the descriptor",
    )
    transport = descriptor.get("transport")
    require(isinstance(transport, dict), f"{label}.transport must be an object")
    repository = require_repository(transport.get("repository"), f"{label}.repository")
    run_id = require_text(transport.get("run_id"), f"{label}.run_id")
    run_attempt = require_text(transport.get("run_attempt"), f"{label}.run_attempt")
    require(repository == args.repository, f"{label}: unexpected repository")
    require(run_id == args.run_id, f"{label}: unexpected source run id")
    require(run_attempt == args.run_attempt, f"{label}: unexpected source run attempt")
    producer = transport.get("producer")
    require(isinstance(producer, dict), f"{label}.producer must be an object")
    require(
        producer.get("workflow_path") == args.expected_producer_workflow,
        f"{label}: unexpected producer workflow",
    )
    require(
        producer.get("revision") == args.expected_producer_revision,
        f"{label}: unexpected producer revision",
    )
    require(
        producer.get("run_id") == args.expected_producer_run_id,
        f"{label}: unexpected producer run id",
    )
    require(
        producer.get("run_attempt") == args.expected_producer_run_attempt,
        f"{label}: unexpected producer run attempt",
    )

    dataset = descriptor.get("dataset")
    require(isinstance(dataset, dict), f"{label}.dataset must be an object")
    job = require_text(dataset.get("job_id"), f"{label}.job_id")
    role = role_for_job(job)
    require(dataset.get("role") == role, f"{label}: role must match the job")
    shard = dataset.get("shard")
    require(isinstance(shard, dict), f"{label}.shard must be an object")
    index = require_integer(shard.get("index"), f"{label}.index", 0)
    count = require_integer(shard.get("count"), f"{label}.count", 1)
    require(
        shard.get("method") == SHARD_METHOD,
        f"{label}: unexpected shard method",
    )
    require(
        count == args.expected_shards,
        f"{label}: shard count must be {args.expected_shards}",
    )
    require(index < count, f"{label}: shard index is out of range")
    source_revision = require_revision(
        dataset.get("source_revision"), f"{label}.source_revision"
    )
    require(
        source_revision == args.source_revision,
        f"{label}: unexpected source revision",
    )
    require(
        dataset.get("physical_job_id") == f"{job}-shard-{index}",
        f"{label}: physical job identity is inconsistent",
    )
    runner = require_digest(
        dataset.get("runner_sha256"), f"{label}.runner_sha256"
    )
    data_sha = require_digest(
        dataset.get("data_sha256"), f"{label}.data_sha256"
    )
    data_bytes = require_integer(
        dataset.get("data_bytes"), f"{label}.data_bytes", 1
    )

    data_path = directory / DATA
    require(
        data_path.is_file() and not data_path.is_symlink(),
        f"{label}: data.json is missing",
    )
    actual_sha, actual_size = hash_file(data_path, f"{label} data.json")
    require(actual_sha == data_sha, f"{label}: data.json digest changed")
    require(actual_size == data_bytes, f"{label}: data.json size changed")
    data = load_json(data_path, f"{label} data.json")
    host_runner = validate_dataset_identity(
        data,
        job=job,
        role=role,
        index=index,
        count=count,
        run_id=args.run_id,
        run_attempt=args.run_attempt,
        source_revision=args.source_revision,
        label=label,
    )
    require(
        dataset.get("host_runner_identity") == host_runner,
        f"{label}: descriptor host runner contradicts retained metadata",
    )
    closure, collections = dataset_closure(data, label=label)

    rows = descriptor.get("files")
    require(isinstance(rows, list), f"{label}.files must be a list")
    declared = {}
    for position, row in enumerate(rows):
        row_label = f"{label}.files[{position}]"
        require(isinstance(row, dict), f"{row_label} must be an object")
        relative = safe_relative(row.get("path"), f"{row_label}.path")
        name = str(relative)
        require(name not in declared, f"{row_label}: duplicate path {name!r}")
        role_name = require_text(row.get("role"), f"{row_label}.role")
        require(
            role_name
            in {
                "raw_stdout",
                "raw_stderr",
                "collection_receipt",
                "collection_lifecycle",
            },
            f"{row_label}: unexpected file role {role_name!r}",
        )
        declared[name] = {
            "role": role_name,
            "sha256": require_digest(row.get("sha256"), f"{row_label}.sha256"),
            "bytes": require_integer(row.get("bytes"), f"{row_label}.bytes", 0),
        }
    require(list(declared) == sorted(declared), f"{label}: files must be sorted")
    expected_roles = dict(closure)
    expected_roles[LIFECYCLE] = "collection_lifecycle"
    require(
        {name: row["role"] for name, row in declared.items()} == expected_roles,
        f"{label}: declared files do not equal the dataset dependency closure",
    )
    require(
        "data.json" not in declared,
        f"{label}: data.json must be described by dataset, not files",
    )

    on_disk = regular_files(directory)
    expected_files = set(declared) | {DATA, PUBLICATION_COPY, DESCRIPTOR}
    require(
        set(on_disk) == expected_files,
        f"{label}: compact input contains missing or extra files: "
        f"missing {sorted(expected_files - set(on_disk))}, "
        f"extra {sorted(set(on_disk) - expected_files)}",
    )
    for relative, row in declared.items():
        sha, size = hash_file(on_disk[relative], f"{label} {relative}")
        require(sha == row["sha256"], f"{label}: {relative} digest changed")
        require(size == row["bytes"], f"{label}: {relative} size changed")

    publication_path = directory / PUBLICATION_COPY
    publication_sha, _ = hash_file(
        publication_path, f"{label} publication manifest"
    )
    source_evidence = descriptor.get("source_evidence")
    require(
        isinstance(source_evidence, dict),
        f"{label}.source_evidence must be an object",
    )
    require(
        source_evidence.get("publication_manifest_path") == PUBLICATION_COPY,
        f"{label}: publication manifest path is inconsistent",
    )
    require(
        source_evidence.get("publication_manifest_sha256") == publication_sha,
        f"{label}: publication manifest digest changed",
    )
    publication = parse_json(
        publication_path.read_bytes(), f"{label} publication manifest"
    )
    published = publication_index(publication, f"{label} publication manifest")
    require(
        source_evidence.get("publication_status") == "published",
        f"{label}: publication status is not published",
    )
    require(
        source_evidence.get("publication_file_count") == len(published),
        f"{label}: publication file count changed",
    )
    require(
        source_evidence.get("completed_samples")
        == len(publication["completed_samples"]),
        f"{label}: completed sample count changed",
    )
    require(
        source_evidence.get("incomplete_samples") == 0,
        f"{label}: incomplete samples are not allowed",
    )
    verify_publication_entry(
        published, DATA, data_sha, data_bytes, ("metadata",), label
    )
    for relative, row in declared.items():
        category = (
            "metadata" if relative == LIFECYCLE else "completed-raw"
        )
        verify_publication_entry(
            published,
            relative,
            row["sha256"],
            row["bytes"],
            (category,),
            label,
        )

    receipts_runner = collection_receipts(
        _CompactDirectorySource(directory, on_disk),
        collections,
        label,
    )
    require(
        receipts_runner == runner,
        f"{label}: collection receipts pin a different runner",
    )
    require(
        dataset.get("samples") == len(data["samples"]),
        f"{label}: sample count changed",
    )
    require(
        dataset.get("pairs") == len(collections),
        f"{label}: pair count changed",
    )

    source_format = require_text(
        source_evidence.get("format"), f"{label}.source_evidence.format"
    )
    expected_name = expected_artifact_name(
        args.run_id, args.run_attempt, job, index
    )
    require(
        source_evidence.get("artifact_name") == expected_name,
        f"{label}: source artifact name must be {expected_name!r}",
    )
    if args.expected_source_format is not None:
        require(
            source_format == args.expected_source_format,
            f"{label}: source format must be {args.expected_source_format!r}",
        )
    if source_format in ("artifact-zip", "sealed-tree"):
        artifact_id = require_integer(
            source_evidence.get("artifact_id"),
            f"{label}.artifact_id",
            1,
        )
        digest = normalize_artifact_digest(
            source_evidence.get("artifact_digest"),
            f"{label}: artifact digest",
        )
        if source_format == "artifact-zip":
            require_integer(
                source_evidence.get("artifact_size"),
                f"{label}.artifact_size",
                1,
            )
            zip_sha = require_digest(
                source_evidence.get("zip_sha256"), f"{label}.zip_sha256"
            )
            require(
                SHA256_TAG + zip_sha == digest,
                f"{label}: ZIP digest and artifact digest disagree",
            )
            zip_bytes = require_integer(
                source_evidence.get("zip_bytes"), f"{label}.zip_bytes", 1
            )
            require(
                zip_bytes == source_evidence["artifact_size"],
                f"{label}: ZIP byte size and artifact size disagree",
            )
        observations["artifact_ids"].append(artifact_id)
        observations["artifact_digests"].append(digest)
    else:
        raise InputError(f"{label}: unsupported source format {source_format!r}")

    observations["shards"].append(
        {
            "path": descriptor_path.relative_to(args.root).as_posix(),
            "job_id": job,
            "index": index,
            "format": source_format,
            "artifact_name": expected_name,
            "input_sha256": recorded,
            "runner_sha256": runner,
        }
    )
    observations["jobs"].setdefault(job, []).append(index)
    observations["pairs"] += len(collections)
    observations["samples"] += len(data["samples"])
    observations["runners"].add(runner)
    observations["revisions"].add(source_revision)
    observations["repositories"].add(repository)


class _CompactDirectorySource:
    """Read a verified compact input from paths already checked by verify."""

    format = "compact"

    def __init__(self, root, files):
        self.root = Path(root)
        self.files = files

    def read(self, relative):
        require(
            relative in self.files,
            f"compact input is missing {relative!r}",
        )
        return read_regular(self.files[relative], f"compact input {relative}")


def command_verify(args):
    args.root = Path(args.root)
    summary = {
        "schema_version": SCHEMA_VERSION,
        "kind": VERIFICATION_KIND,
        "status": "failed",
        "root": str(args.root),
        "expected": {
            "repository": args.repository,
            "run_id": args.run_id,
            "run_attempt": args.run_attempt,
            "source_revision": args.source_revision,
            "discovery_jobs": args.expected_discovery_jobs,
            "validation_jobs": args.expected_validation_jobs,
            "shards": args.expected_shards,
            "source_format": args.expected_source_format,
            "producer_workflow": args.expected_producer_workflow,
            "producer_revision": args.expected_producer_revision,
        },
        "observed": {},
        "shards": [],
        "errors": [],
    }
    errors = summary["errors"]
    try:
        require(args.root.is_dir(), f"verification root is missing: {args.root}")
        descriptors = descriptor_paths(args.root)
        require(
            descriptors,
            "no report-input.json descriptors found",
        )
        expected_total = (
            args.expected_discovery_jobs + args.expected_validation_jobs
        ) * args.expected_shards
        require(
            len(descriptors) == expected_total,
            f"expected {expected_total} compact shard inputs, "
            f"found {len(descriptors)}",
        )
        observations = {
            "shards": [],
            "jobs": {},
            "pairs": 0,
            "samples": 0,
            "runners": set(),
            "revisions": set(),
            "repositories": set(),
            "artifact_ids": [],
            "artifact_digests": [],
        }
        for descriptor in descriptors:
            try:
                verify_descriptor(
                    descriptor,
                    args=args,
                    observations=observations,
                )
            except InputError as error:
                errors.append(str(error))
        if observations["shards"]:
            require(
                len(observations["artifact_ids"])
                == len(set(observations["artifact_ids"])),
                "source artifact ids must be unique",
            )
            require(
                len(observations["artifact_digests"])
                == len(set(observations["artifact_digests"])),
                "source artifact digests must be unique",
            )
            require(
                len(observations["runners"]) == 1,
                "all physical shards must pin the same original runner",
            )
            require(
                observations["revisions"] == {args.source_revision},
                "all physical shards must use the pinned source revision",
            )
            require(
                observations["repositories"] == {args.repository},
                "all physical shards must use the pinned repository",
            )
            require(
                set(observations["jobs"]) == set(JOBS),
                f"logical jobs must be exactly {list(JOBS)}",
            )
            for job in JOBS:
                indices = sorted(observations["jobs"].get(job, []))
                require(
                    indices == list(range(args.expected_shards)),
                    f"{job}: shard indices must be complete and unique, "
                    f"found {indices}",
                )
            roles = [role_for_job(job) for job in observations["jobs"]]
            require(
                roles.count("discovery") == args.expected_discovery_jobs,
                "discovery job count changed",
            )
            require(
                roles.count("validation") == args.expected_validation_jobs,
                "validation job count changed",
            )
        data_files = []
        for current, _, names in os.walk(args.root, followlinks=False):
            if DATA in names:
                data_files.append(Path(current) / DATA)
        descriptor_dirs = {path.parent.resolve() for path in descriptors}
        for data_path in data_files:
            require(
                data_path.parent.resolve() in descriptor_dirs,
                f"undescribed data.json found: {data_path}",
            )
        summary["observed"] = {
            "compact_shards": len(observations["shards"]),
            "jobs": {
                job: sorted(observations["jobs"].get(job, []))
                for job in sorted(observations["jobs"])
            },
            "pairs": observations["pairs"],
            "samples": observations["samples"],
            "runner_sha256": (
                next(iter(observations["runners"]))
                if observations["runners"]
                else None
            ),
            "source_formats": sorted(
                {row["format"] for row in observations["shards"]}
            ),
        }
        summary["shards"] = observations["shards"]
        if errors:
            raise InputError(
                f"{len(errors)} compact input invariant(s) failed"
            )
        summary["status"] = "passed"
    except InputError as error:
        if str(error) not in errors:
            errors.append(str(error))
    if args.json_output:
        write_json(args.json_output, summary)
    if summary["status"] == "passed":
        print(
            f"verified {len(summary['shards'])} compact shard inputs, "
            f"{summary['observed']['pairs']} pairs, "
            f"runner {summary['observed']['runner_sha256']}"
        )
        return 0
    for error in summary["errors"]:
        print(f"error: {error}", file=sys.stderr)
    print(
        f"compact report input verification failed with "
        f"{len(summary['errors'])} error(s)",
        file=sys.stderr,
    )
    return 1


def parser():
    root = argparse.ArgumentParser(
        description=(
            "Publish and verify compact calibration report inputs without "
            "duplicating the authoritative Rust validator."
        )
    )
    commands = root.add_subparsers(dest="command", required=True)

    build = commands.add_parser(
        "build", help="publish a compact input from a sealed evidence tree"
    )
    build.add_argument("--sealed", required=True)
    build.add_argument("--output", required=True)
    build.add_argument("--repository", required=True)
    build.add_argument("--run-id", required=True)
    build.add_argument("--run-attempt", required=True)
    build.add_argument("--logical-job", required=True, choices=JOBS)
    build.add_argument("--shard-index", required=True, type=int)
    build.add_argument("--shard-count", required=True, type=int)
    build.add_argument("--expected-source-revision", required=True)
    build.add_argument("--expected-runner", required=True)
    build.add_argument("--source-artifact-id", required=True, type=int)
    build.add_argument("--source-artifact-digest", required=True)
    build.add_argument("--producer-workflow", required=True)
    build.add_argument("--producer-revision", required=True)
    build.add_argument("--producer-run-id", required=True)
    build.add_argument("--producer-run-attempt", required=True)
    build.set_defaults(func=command_build)

    plan = commands.add_parser(
        "plan", help="pin the original run's four artifacts before download"
    )
    plan.add_argument("--run-json", required=True)
    plan.add_argument("--artifacts-json", required=True)
    plan.add_argument("--repository", required=True)
    plan.add_argument("--run-id", required=True)
    plan.add_argument("--run-attempt", required=True)
    plan.add_argument("--source-revision", required=True)
    plan.add_argument("--logical-job", required=True, choices=JOBS)
    plan.add_argument("--shard-count", required=True, type=int)
    plan.add_argument("--output", required=True)
    plan.set_defaults(func=command_plan)

    export = commands.add_parser(
        "export", help="copy the dependency closure out of original ZIP bytes"
    )
    export.add_argument("--plan", required=True)
    export.add_argument("--raw-dir", required=True)
    export.add_argument("--output", required=True)
    export.add_argument("--producer-workflow", required=True)
    export.add_argument("--producer-revision", required=True)
    export.add_argument("--producer-run-id", required=True)
    export.add_argument("--producer-run-attempt", required=True)
    export.set_defaults(func=command_export)

    verify = commands.add_parser(
        "verify", help="fail closed unless every compact input is complete"
    )
    verify.add_argument("--root", required=True)
    verify.add_argument("--repository", required=True)
    verify.add_argument("--run-id", required=True)
    verify.add_argument("--run-attempt", required=True)
    verify.add_argument("--source-revision", required=True)
    verify.add_argument("--expected-producer-run-id", required=True)
    verify.add_argument("--expected-producer-run-attempt", required=True)
    verify.add_argument("--expected-producer-workflow", required=True)
    verify.add_argument("--expected-producer-revision", required=True)
    verify.add_argument("--expected-discovery-jobs", required=True, type=int)
    verify.add_argument("--expected-validation-jobs", required=True, type=int)
    verify.add_argument("--expected-shards", required=True, type=int)
    verify.add_argument(
        "--expected-source-format",
        choices=("sealed-tree", "artifact-zip"),
        default=None,
    )
    verify.add_argument("--json-output", default=None)
    verify.set_defaults(func=command_verify)
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    try:
        return args.func(args)
    except InputError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except OSError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
