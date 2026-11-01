"""Bounded JSON-RPC stdio client for semantic consumers and recorded replay."""
import io
import json
import os
import queue
import select
import subprocess
import tempfile
import threading
import time

REQUEST_TIMEOUT = 30.0
MAX_FRAME_BYTES = 16 * 1024 * 1024
REMOVED_ENVIRONMENT = ("BEND_LIB", "BEND2_LSP_COMPILER_METRICS_FILE", "BEND2_LSP_TRACE")


class MeasurementError(Exception):
    """A transport, protocol, or process-lifecycle failure."""


def reject_non_json_constant(value):
    raise ValueError(f"non-JSON numeric constant: {value}")


def frame(message):
    body = json.dumps(
        message, separators=(",", ":"), ensure_ascii=False, allow_nan=False,
    ).encode()
    return f"Content-Length: {len(body)}\r\n\r\n".encode("ascii") + body


class LspProcess:
    """Validate framed replies while serving the server's client requests."""

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
        # Exclude ambient compilers and the implicit ~/.bend/lib lookup while
        # initialize/configuration notifications are still being applied.
        environment.update(PATH=str(empty_path), HOME=str(home), USERPROFILE=str(home))
        self.stderr = tempfile.TemporaryFile(mode="w+b")
        try:
            self.process = subprocess.Popen(
                [str(binary)], cwd=workspace, env=environment,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, bufsize=0,
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
            "client/registerCapability", "client/unregisterCapability",
            "window/workDoneProgress/create", "window/showMessageRequest",
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
                result.append(self.settings if section is None else self.settings.get(section))
        elif method == "workspace/workspaceFolders":
            result = [{"uri": self.workspace.as_uri(), "name": "semantic"}]
        else:
            self.write_frame(frame({
                "jsonrpc": "2.0", "id": message["id"],
                "error": {"code": -32601, "message": "Method not supported by semantic client"},
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
                    not isinstance(params, dict) or not isinstance(params.get("uri"), str)
                    or not isinstance(params.get("diagnostics"), list)
                    or any(not isinstance(item, dict) for item in params["diagnostics"])
                    or (params.get("version") is not None and type(params["version"]) is not int)
                ):
                    raise MeasurementError("malformed publishDiagnostics notification")
                self.diagnostics[params["uri"]] = params
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

    def initialize(self):
        result, _ = self.request("initialize", {
            "processId": None, "rootUri": self.workspace.as_uri(), "capabilities": {},
            "workspaceFolders": [{"uri": self.workspace.as_uri(), "name": "semantic"}],
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
