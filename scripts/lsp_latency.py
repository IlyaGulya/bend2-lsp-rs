#!/usr/bin/env python3
"""Measure paired real stdio LSP processes; analysis/protocol, not Bend execution."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import queue
import select
import subprocess
import sys
import tempfile
import threading
import time

WORKLOADS = (
    "hover_warm",
    "definition_warm",
    "completion_warm",
    "open_to_hover_large",
    "edit_to_hover_large",
    "hover_during_large_edit",
    "hover_during_large_open",
)
FIXTURE_REPETITIONS = 4
REQUEST_TIMEOUT = 30.0
MAX_FRAME_BYTES = 16 * 1024 * 1024
REMOVED_ENVIRONMENT = (
    "BEND_LIB",
    "BEND2_LSP_COMPILER_METRICS_FILE",
    "BEND2_LSP_TRACE",
)
ADT_SOURCE = (
    "type LatencyTerm is Data:\n"
    "  TermVar{index: Nat}\n"
    "  TermRef{name: String}\n"
)
SMALL_SOURCE = (
    "def add(x: U32, y: U32) -> U32:\n"
    "  (x + y : U32)\n"
    "def main: U32\n"
    "  add(1, 2)\n"
) + ADT_SOURCE
COMPLETION_SOURCE = SMALL_SOURCE.replace("  add(1, 2)\n", "  ad\n")
DEPENDENCY_SOURCE = "def clamp(x: U32) -> U32:\n  x\n" + ADT_SOURCE
IMPORTER_SOURCE = (
    "import dep.bend as Dep\ndef main: U32\n  Dep.clamp(1)\n" + ADT_SOURCE
)
SMALL_SIGNATURE = "def add(x: U32, y: U32) -> U32"


class MeasurementError(Exception):
    """A transport, protocol, semantic, or process-lifecycle failure."""


def reject_non_json_constant(value):
    raise ValueError(f"non-JSON numeric constant: {value}")


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def frame(message):
    body = json.dumps(
        message, separators=(",", ":"), ensure_ascii=False, allow_nan=False,
    ).encode()
    return f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body


def position(uri, line, character):
    return {
        "textDocument": {"uri": uri},
        "position": {"line": line, "character": character},
    }


def open_message(uri, source, version=1):
    return {
        "jsonrpc": "2.0",
        "method": "textDocument/didOpen",
        "params": {
            "textDocument": {
                "uri": uri,
                "languageId": "bend",
                "version": version,
                "text": source,
            }
        },
    }


def change_message(uri, source, version):
    return {
        "jsonrpc": "2.0",
        "method": "textDocument/didChange",
        "params": {
            "textDocument": {"uri": uri, "version": version},
            "contentChanges": [{"text": source}],
        },
    }


def require_hover(result, signature, previous=None):
    contents = result.get("contents") if isinstance(result, dict) else None
    value = contents.get("value") if isinstance(contents, dict) else None
    if not isinstance(value, str) or signature not in value:
        raise MeasurementError(f"hover did not expose {signature!r}: {result!r}")
    if previous is not None and previous in value:
        raise MeasurementError(f"hover exposed stale revision {previous!r}: {result!r}")


def require_definition(result, uri):
    expected = {
        "uri": uri,
        "range": {
            "start": {"line": 0, "character": 4},
            "end": {"line": 0, "character": 9},
        },
    }
    if result != expected:
        raise MeasurementError(f"definition expected {expected!r}, received {result!r}")


def require_completion(result):
    items = result.get("items") if isinstance(result, dict) else result
    if not isinstance(items, list) or not any(
        isinstance(item, dict) and item.get("label") == "add" for item in items
    ):
        raise MeasurementError(f"completion did not offer the known add declaration: {result!r}")


class LspProcess:
    """Read bodies on a dedicated thread so pipelined receipt times exclude parsing."""

    def __init__(self, binary, workspace):
        self.workspace = workspace
        self.compiler_path = str(workspace / "unavailable-compiler" / "bend")
        self.settings = {
            "bend2-lsp": {"compilerPath": self.compiler_path, "compilerArguments": []}
        }
        empty_path = workspace / "empty-path"
        empty_path.mkdir()
        home = workspace / "empty-home"
        home.mkdir()
        environment = os.environ.copy()
        for key in REMOVED_ENVIRONMENT:
            environment.pop(key, None)
        # An empty PATH also excludes the default `bend` while configuration is
        # being applied. An empty home excludes the implicit ~/.bend/lib lookup.
        environment.update(PATH=str(empty_path), HOME=str(home), USERPROFILE=str(home))
        self.stderr = tempfile.TemporaryFile(mode="w+b")
        try:
            self.process = subprocess.Popen(
                [str(binary)],
                cwd=workspace,
                env=environment,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=self.stderr,
                bufsize=0,
            )
        except BaseException:
            self.stderr.close()
            raise
        try:
            self.messages = queue.Queue()
            self.pending = {}
            self.responses = {}
            self.diagnostics = {}
            self.next_id = 1
            os.set_blocking(self.process.stdin.fileno(), False)
            self.reader = threading.Thread(target=self._read_frames, daemon=True)
            self.reader.start()
        except BaseException:
            self.process.kill()
            self.process.wait()
            self.process.stdin.close()
            self.process.stdout.close()
            self.stderr.close()
            raise

    def _read_frames(self):
        try:
            reader = io.BufferedReader(self.process.stdout)
            while True:
                content_length = None
                header_bytes = 0
                while True:
                    line = reader.readline(8193)
                    if not line:
                        if header_bytes:
                            raise MeasurementError("EOF inside an LSP header")
                        self.messages.put((None, 0))
                        return
                    header_bytes += len(line)
                    if len(line) > 8192 or header_bytes > 16384:
                        raise MeasurementError("oversized LSP header")
                    if line == b"\r\n":
                        break
                    if not line.endswith(b"\r\n") or b":" not in line:
                        raise MeasurementError(f"invalid LSP header: {line!r}")
                    name, value = line[:-2].split(b":", 1)
                    if name.lower() == b"content-length":
                        value = value.strip()
                        if content_length is not None or not value.isdigit():
                            raise MeasurementError("invalid or duplicate Content-Length")
                        content_length = int(value)
                if content_length is None or not 0 < content_length <= MAX_FRAME_BYTES:
                    raise MeasurementError("missing or invalid LSP Content-Length")
                body = reader.read(content_length)
                received_ns = time.perf_counter_ns()
                # Timestamp immediately after the complete body read, before
                # decoding, JSON parsing, validation, or response dispatch.
                if len(body) != content_length:
                    raise MeasurementError("EOF inside an LSP body")
                self.messages.put((body, received_ns))
        except Exception as error:
            self.messages.put((error, 0))

    def write_frame(self, data):
        deadline = time.monotonic() + REQUEST_TIMEOUT
        view = memoryview(data)
        while view:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise MeasurementError("timed out writing an LSP frame")
            _, writable, _ = select.select([], [self.process.stdin], [], remaining)
            if not writable:
                raise MeasurementError("timed out writing an LSP frame")
            try:
                written = os.write(self.process.stdin.fileno(), view)
            except BlockingIOError:
                continue
            if not written:
                raise MeasurementError("LSP stdin accepted no bytes")
            view = view[written:]

    def notify(self, method, params):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.write_frame(frame(message))

    def prepare_request(self, method, params):
        request_id = self.next_id
        self.next_id += 1
        message = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        return request_id, method, frame(message)

    def send_request(self, prepared, notification=None):
        request_id, method, data = prepared
        self.pending[request_id] = method
        started_ns = time.perf_counter_ns()
        if notification is not None:
            self.write_frame(notification)
        self.write_frame(data)
        return request_id, started_ns

    def _server_request(self, message):
        method = message["method"]
        params = message.get("params", {})
        if method in (
            "client/registerCapability",
            "client/unregisterCapability",
            "window/workDoneProgress/create",
            "window/showMessageRequest",
        ):
            result = None
        elif method == "workspace/configuration":
            if not isinstance(params, dict) or not isinstance(params.get("items"), list):
                raise MeasurementError("malformed workspace/configuration request")
            result = []
            for item in params["items"]:
                if not isinstance(item, dict):
                    raise MeasurementError("malformed workspace/configuration item")
                section = item.get("section")
                result.append(
                    self.settings if section is None else self.settings.get(section)
                )
        elif method == "workspace/workspaceFolders":
            result = [{"uri": self.workspace.as_uri(), "name": "latency"}]
        else:
            self.write_frame(frame({
                "jsonrpc": "2.0", "id": message["id"],
                "error": {"code": -32601, "message": "Method not supported by latency client"},
            }))
            raise MeasurementError(f"unexpected server request: {method}")
        self.write_frame(frame({"jsonrpc": "2.0", "id": message["id"], "result": result}))

    def _dispatch(self, body, received_ns):
        try:
            message = json.loads(body, parse_constant=reject_non_json_constant)
        except (ValueError, UnicodeError) as error:
            raise MeasurementError(f"invalid JSON from LSP: {error}") from error
        if not isinstance(message, dict) or message.get("jsonrpc") != "2.0":
            raise MeasurementError(f"invalid JSON-RPC message: {message!r}")
        if "method" in message:
            if not isinstance(message["method"], str) or "result" in message or "error" in message:
                raise MeasurementError(f"invalid JSON-RPC call: {message!r}")
            if "id" in message:
                if type(message["id"]) not in (int, str):
                    raise MeasurementError("invalid server request ID")
                self._server_request(message)
            elif message["method"] == "textDocument/publishDiagnostics":
                params = message.get("params")
                if (
                    not isinstance(params, dict)
                    or not isinstance(params.get("uri"), str)
                    or not isinstance(params.get("diagnostics"), list)
                    or any(not isinstance(item, dict) for item in params["diagnostics"])
                    or (params.get("version") is not None and type(params["version"]) is not int)
                ):
                    raise MeasurementError("malformed publishDiagnostics notification")
                self.diagnostics[params["uri"]] = params
            # Other asynchronous notifications do not complete client requests.
            return
        request_id = message.get("id")
        if type(request_id) is not int or request_id not in self.pending:
            raise MeasurementError(f"unexpected LSP response ID: {request_id!r}")
        if request_id in self.responses:
            raise MeasurementError(f"duplicate LSP response ID: {request_id}")
        if "error" in message:
            raise MeasurementError(f"{self.pending[request_id]} failed: {message['error']!r}")
        if "result" not in message:
            raise MeasurementError(f"LSP response has no result: {message!r}")
        self.responses[request_id] = (message["result"], received_ns)

    def _receive(self, deadline):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise MeasurementError("timed out waiting for LSP output")
        try:
            body, received_ns = self.messages.get(timeout=remaining)
        except queue.Empty as error:
            raise MeasurementError("timed out waiting for LSP output") from error
        if body is None:
            raise MeasurementError("unexpected EOF from LSP")
        if isinstance(body, Exception):
            raise MeasurementError(f"LSP transport failed: {body}") from body
        self._dispatch(body, received_ns)

    def response(self, request_id, started_ns):
        deadline = time.monotonic() + REQUEST_TIMEOUT
        while request_id not in self.responses:
            self._receive(deadline)
        result, received_ns = self.responses.pop(request_id)
        del self.pending[request_id]
        elapsed = received_ns - started_ns
        if elapsed <= 0:
            raise MeasurementError("non-positive request duration")
        return result, elapsed

    def request(self, method, params, notification=None):
        prepared = self.prepare_request(method, params)
        return self.response(*self.send_request(prepared, notification))

    def wait_diagnostics(self, uri, version):
        deadline = time.monotonic() + REQUEST_TIMEOUT
        while True:
            params = self.diagnostics.get(uri)
            if params is not None and params.get("version") == version:
                if version is None:
                    if params["diagnostics"]:
                        raise MeasurementError("didClose did not clear diagnostics")
                elif not any(
                    item.get("code") == "compiler-unavailable"
                    and self.compiler_path in item.get("message", "")
                    for item in params["diagnostics"]
                ):
                    raise MeasurementError(
                        f"revision {version} did not confirm the unavailable compiler: {params!r}"
                    )
                return
            self._receive(deadline)

    def close_document(self, uri):
        self.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
        self.wait_diagnostics(uri, None)
        self.diagnostics.pop(uri, None)

    def initialize(self):
        result, _ = self.request("initialize", {
            "processId": None,
            "rootUri": self.workspace.as_uri(),
            "capabilities": {},
            "workspaceFolders": [{"uri": self.workspace.as_uri(), "name": "latency"}],
        })
        if not isinstance(result, dict) or not isinstance(result.get("capabilities"), dict):
            raise MeasurementError("initialize did not return server capabilities")
        self.notify("initialized", {})
        self.notify("workspace/didChangeConfiguration", {"settings": self.settings})

    def finish(self):
        result, _ = self.request("shutdown", None)
        if result is not None:
            raise MeasurementError(f"shutdown returned a non-null result: {result!r}")
        self.notify("exit", None)
        self.process.stdin.close()
        try:
            status = self.process.wait(timeout=REQUEST_TIMEOUT)
        except subprocess.TimeoutExpired as error:
            raise MeasurementError("LSP did not exit after shutdown") from error
        if status != 0:
            raise MeasurementError(f"LSP exited with status {status}")
        # Do not miss malformed trailing output just because shutdown responded.
        deadline = time.monotonic() + REQUEST_TIMEOUT
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise MeasurementError("LSP stdout remained open after process exit")
            try:
                body, received_ns = self.messages.get(timeout=remaining)
            except queue.Empty as error:
                raise MeasurementError("LSP stdout remained open after process exit") from error
            if body is None:
                break
            if isinstance(body, Exception):
                raise MeasurementError(f"LSP transport failed during shutdown: {body}") from body
            self._dispatch(body, received_ns)

    def stderr_tail(self):
        self.stderr.seek(0, os.SEEK_END)
        self.stderr.seek(max(0, self.stderr.tell() - 8192))
        return self.stderr.read().decode("utf-8", errors="replace").strip()

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait()
        if not self.process.stdin.closed:
            self.process.stdin.close()
        self.reader.join(timeout=REQUEST_TIMEOUT)
        self.process.stdout.close()
        self.stderr.close()


def large_revision(body, workload, revision):
    name = f"latency_{workload}_revision_{revision}"
    return name, f"def {name}: U32\n  {revision}\n{ADT_SOURCE}{body}"


def warm_queries(client, method, params, check, samples, warmup):
    measured = []
    for index in range(warmup + samples):
        result, elapsed = client.request(method, params)
        check(result)
        if index >= warmup:
            measured.append(elapsed)
    return measured


def pipelined_hovers(client, uri, count, notification):
    prepared = [
        client.prepare_request("textDocument/hover", position(uri, 3, 4))
        for _ in range(count)
    ]
    client.write_frame(notification)
    pending = [client.send_request(request) for request in prepared]
    measured = []
    for request_id, started_ns in pending:
        result, elapsed = client.response(request_id, started_ns)
        require_hover(result, SMALL_SIGNATURE)
        measured.append(elapsed)
    return measured


def measure_round(binary, body, samples, warmup):
    with tempfile.TemporaryDirectory(prefix="bend-lsp-latency-") as temporary:
        workspace = Path(temporary).resolve()
        documents = {
            "small": SMALL_SOURCE,
            "completion": COMPLETION_SOURCE,
            "dep": DEPENDENCY_SOURCE,
            "importer": IMPORTER_SOURCE,
        }
        uris = {}
        for name, source in documents.items():
            path = workspace / f"{name}.bend"
            path.write_text(source, encoding="utf-8")
            uris[name] = path.as_uri()
        client = LspProcess(binary, workspace)
        try:
            client.initialize()
            # Settle every small document and the actual missing-compiler path
            # before any workload. Neither process startup nor initial indexing
            # belongs to a warm-request latency sample.
            for name, source in documents.items():
                client.write_frame(frame(open_message(uris[name], source)))
                client.wait_diagnostics(uris[name], 1)
            result, _ = client.request("textDocument/hover", position(uris["small"], 3, 4))
            require_hover(result, SMALL_SIGNATURE)
            result, _ = client.request("textDocument/definition", position(uris["importer"], 2, 8))
            require_definition(result, uris["dep"])
            result, _ = client.request("textDocument/completion", position(uris["completion"], 3, 4))
            require_completion(result)

            measured = {}
            measured["hover_warm"] = warm_queries(
                client, "textDocument/hover", position(uris["small"], 3, 4),
                lambda value: require_hover(value, SMALL_SIGNATURE), samples, warmup,
            )
            measured["definition_warm"] = warm_queries(
                client, "textDocument/definition", position(uris["importer"], 2, 8),
                lambda value: require_definition(value, uris["dep"]), samples, warmup,
            )
            measured["completion_warm"] = warm_queries(
                client, "textDocument/completion", position(uris["completion"], 3, 4),
                require_completion, samples, warmup,
            )

            workload = "open_to_hover_large"
            measured[workload] = []
            for revision in range(1, warmup + samples + 1):
                uri = (workspace / f"{workload}_{revision}.bend").as_uri()
                name, source = large_revision(body, workload, revision)
                notification = frame(open_message(uri, source))
                result, elapsed = client.request(
                    "textDocument/hover", position(uri, 0, 5), notification,
                )
                require_hover(result, f"def {name}: U32")
                if revision > warmup:
                    measured[workload].append(elapsed)
                client.wait_diagnostics(uri, 1)
                client.close_document(uri)

            workload = "edit_to_hover_large"
            uri = (workspace / f"{workload}.bend").as_uri()
            previous, source = large_revision(body, workload, 1)
            client.write_frame(frame(open_message(uri, source)))
            result, _ = client.request("textDocument/hover", position(uri, 0, 5))
            require_hover(result, f"def {previous}: U32")
            client.wait_diagnostics(uri, 1)
            measured[workload] = []
            for index in range(warmup + samples):
                revision = index + 2
                name, source = large_revision(body, workload, revision)
                notification = frame(change_message(uri, source, revision))
                result, elapsed = client.request(
                    "textDocument/hover", position(uri, 0, 5), notification,
                )
                require_hover(result, f"def {name}: U32", previous)
                if index >= warmup:
                    measured[workload].append(elapsed)
                client.wait_diagnostics(uri, revision)
                previous = name
            client.close_document(uri)

            workload = "hover_during_large_edit"
            uri = (workspace / f"{workload}.bend").as_uri()
            previous, source = large_revision(body, workload, 1)
            client.write_frame(frame(open_message(uri, source)))
            result, _ = client.request("textDocument/hover", position(uri, 0, 5))
            require_hover(result, f"def {previous}: U32")
            client.wait_diagnostics(uri, 1)
            revision = 1
            # The busy workloads are one burst of N requests, not N edits. A
            # separate warm-up burst has the same interfering full notification.
            for count in ([warmup, samples] if warmup else [samples]):
                revision += 1
                name, source = large_revision(body, workload, revision)
                burst = pipelined_hovers(
                    client, uris["small"], count,
                    frame(change_message(uri, source, revision)),
                )
                result, _ = client.request("textDocument/hover", position(uri, 0, 5))
                require_hover(result, f"def {name}: U32", previous)
                client.wait_diagnostics(uri, revision)
                previous = name
            measured[workload] = burst
            client.close_document(uri)

            workload = "hover_during_large_open"
            for revision, count in enumerate(
                [warmup, samples] if warmup else [samples], start=1,
            ):
                uri = (workspace / f"{workload}_{revision}.bend").as_uri()
                name, source = large_revision(body, workload, revision)
                burst = pipelined_hovers(
                    client, uris["small"], count, frame(open_message(uri, source)),
                )
                result, _ = client.request("textDocument/hover", position(uri, 0, 5))
                require_hover(result, f"def {name}: U32")
                client.wait_diagnostics(uri, 1)
                client.close_document(uri)
            measured[workload] = burst
            client.finish()
            return measured
        except (MeasurementError, OSError) as error:
            stderr = client.stderr_tail()
            detail = f"\nLSP stderr (last 8 KiB):\n{stderr}" if stderr else ""
            raise MeasurementError(f"{error}{detail}") from error
        finally:
            client.close()


def positive_int(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def nonnegative_int(value):
    number = int(value)
    if number < 0:
        raise argparse.ArgumentTypeError("must be non-negative")
    return number


def write_completed_pair(outputs, measurements):
    staged = []
    try:
        # Serialize and stage both completed measurements before publishing any
        # result. Each replacement is atomic; no in-progress sample file exists.
        for output, measurement in zip(outputs, measurements):
            output.parent.mkdir(parents=True, exist_ok=True)
            with tempfile.NamedTemporaryFile(
                mode="w", encoding="utf-8", dir=output.parent,
                prefix=f".{output.name}.", suffix=".tmp", delete=False,
            ) as stream:
                staged.append(Path(stream.name))
                json.dump(measurement, stream, indent=2, allow_nan=False)
                stream.write("\n")
                stream.flush()
                os.fsync(stream.fileno())
        for temporary, output in zip(staged, outputs):
            os.replace(temporary, output)
    finally:
        for temporary in staged:
            temporary.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-binary", required=True, type=Path)
    parser.add_argument("--candidate-binary", required=True, type=Path)
    parser.add_argument("--baseline-output", required=True, type=Path)
    parser.add_argument("--candidate-output", required=True, type=Path)
    parser.add_argument("--rounds", type=positive_int, default=7)
    parser.add_argument("--samples", type=positive_int, default=32)
    parser.add_argument("--warmup", type=nonnegative_int, default=8)
    args = parser.parse_args()
    try:
        harness = Path(__file__).resolve()
        fixture = harness.parent.parent / "benches" / "fixtures" / "analyzer_large.bend"
        binaries = [args.baseline_binary.resolve(), args.candidate_binary.resolve()]
        outputs = [args.baseline_output.resolve(), args.candidate_output.resolve()]
        if outputs[0] == outputs[1]:
            raise MeasurementError("baseline and candidate output paths must differ")
        if any(output in [*binaries, harness, fixture] for output in outputs):
            raise MeasurementError("output paths must not overwrite binaries, harness, or fixture")
        for binary in binaries:
            if not binary.is_file() or not os.access(binary, os.X_OK):
                raise MeasurementError(f"binary is not an executable file: {binary}")
        fixture_bytes = fixture.read_bytes()
        if not fixture_bytes:
            raise MeasurementError("large fixture is empty")
        fixture_text = fixture_bytes.decode("utf-8")
        separator = "" if fixture_text.endswith("\n") else "\n"
        body = (fixture_text + separator) * FIXTURE_REPETITIONS
        parameters = {"rounds": args.rounds, "samples": args.samples, "warmup": args.warmup}
        harness_sha256 = sha256_file(harness)
        fixture_sha256 = hashlib.sha256(fixture_bytes).hexdigest()
        digest_input = {
            "format_version": 1,
            "harness_sha256": harness_sha256,
            "fixture_sha256": fixture_sha256,
            "fixture_repetitions": FIXTURE_REPETITIONS,
            "workloads": WORKLOADS,
            **parameters,
        }
        workload_digest = hashlib.sha256(
            json.dumps(digest_input, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()
        hashes = [sha256_file(binary) for binary in binaries]
        order = [["baseline", "candidate"] if index % 2 == 0 else ["candidate", "baseline"]
                 for index in range(args.rounds)]
        metadata = {
            "platform": platform.platform(),
            "machine": platform.machine(),
            "python": platform.python_version(),
            **parameters,
            "harness_sha256": harness_sha256,
            "fixture_sha256": fixture_sha256,
            "fixture_bytes": len(fixture_bytes),
            "fixture_repetitions": FIXTURE_REPETITIONS,
            "large_body_bytes": len(body.encode()),
            "generated_adt_source": ADT_SOURCE,
            "round_order": order,
            "compiler_configuration": {
                "compilerPath": "<fresh-workspace>/unavailable-compiler/bend (does not exist)",
                "compilerArguments": [],
                "real_compiler_execution": False,
                "PATH": "<fresh-workspace>/empty-path",
                "HOME": "<fresh-workspace>/empty-home",
                "USERPROFILE": "<fresh-workspace>/empty-home",
                "removed_environment": list(REMOVED_ENVIRONMENT),
                "readiness": "semantic response plus compiler-unavailable diagnostics for exact revision",
            },
            "timing": {
                "clock": "time.perf_counter_ns",
                "start": "before frame write; causal open/edit include notification frame write",
                "end": "immediately after full response body read, before JSON parsing",
                "busy_requests": "per-request hover latency; large notification precedes pipelined burst",
                "initialization_measured": False,
                "request_timeout_seconds": REQUEST_TIMEOUT,
            },
        }
        measurements = [
            {
                "format_version": 1,
                "workload_digest": workload_digest,
                "metadata": {**metadata, "binary_sha256": binary_hash},
                "workloads": {workload: {"rounds_ns": []} for workload in WORKLOADS},
            }
            for binary_hash in hashes
        ]
        for round_index, variants in enumerate(order):
            for variant in variants:
                index = 0 if variant == "baseline" else 1
                print(f"round {round_index + 1}/{args.rounds}: {variant}", file=sys.stderr, flush=True)
                try:
                    result = measure_round(binaries[index], body, args.samples, args.warmup)
                except MeasurementError as error:
                    raise MeasurementError(f"round {round_index + 1} {variant}: {error}") from error
                if set(result) != set(WORKLOADS) or any(
                    len(result[name]) != args.samples
                    or any(type(value) is not int or value <= 0 for value in result[name])
                    for name in WORKLOADS
                ):
                    raise MeasurementError("round did not produce every required positive sample")
                for workload in WORKLOADS:
                    measurements[index]["workloads"][workload]["rounds_ns"].append(result[workload])
        if [sha256_file(binary) for binary in binaries] != hashes:
            raise MeasurementError("a measured binary changed during collection")
        if sha256_file(harness) != harness_sha256 or sha256_file(fixture) != fixture_sha256:
            raise MeasurementError("harness or fixture changed during collection")
        write_completed_pair(outputs, measurements)
        return 0
    except (MeasurementError, OSError, UnicodeError, ValueError) as error:
        print(f"latency measurement failed: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("latency measurement interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    sys.exit(main())
