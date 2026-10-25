#!/usr/bin/env python3
"""Persistent native process observations within the supervisor's own deadline."""
from collections.abc import Callable, Iterable, Sequence
import json
import os
import selectors
import subprocess
import time


_MAX_REQUEST = 1024*1024
_MAX_RESPONSE = 16*1024*1024
_MAX_STDERR = 64*1024
_POLL_SECONDS = .02


class ObserverError(RuntimeError):
    """A failed observation or cleanup, including bounded helper diagnostics."""

    def __init__(self, message: str, stderr: str = ""):
        self.reason = message
        self.stderr = stderr
        super().__init__(message+(f"; observer stderr: {stderr.rstrip()}" if stderr else ""))


class ProcessObserver:
    """One lazy helper per observation stage; errors permanently disable it.

    Every failure kills the exact helper immediately. The owner must still call
    close with the original phase deadline to reap it before child CPU totals.
    A distinct shutdown-only verifier may follow; failed work is never resumed.
    This instance creates no groups, threads, retries or replacement helpers.
    """

    def __init__(self, command: Sequence[str]):
        self._command = tuple(command)
        self._process = None
        self._selector = None
        self._failed = False
        self._closed = False
        self._stderr = bytearray()

    @property
    def pid(self) -> int | None:
        return None if self._process is None else self._process.pid

    @property
    def failed(self) -> bool:
        return self._failed

    @property
    def stderr(self) -> str:
        return self._stderr.decode("utf-8", errors="replace")

    @staticmethod
    def _check(deadline: float, cancellable: bool, interrupted: Callable[[], object] | None) -> float:
        if cancellable and interrupted is not None:
            interruption = interrupted()
            if interruption:
                raise ObserverError(f"observer interrupted by {interruption}")
        remaining = deadline-time.monotonic()
        if remaining <= 0:
            raise ObserverError("hard deadline exceeded during process observation")
        return remaining

    @staticmethod
    def _pid(value: object) -> bool:
        return type(value) is int and 0 < value <= 0xffffffff

    def _request(self, root: int, tracked: Iterable[int], groups: Iterable[int], check: Callable[[], float]) -> bytearray:
        if not self._pid(root):
            raise ObserverError("observer root must be a positive u32 PID")
        request = bytearray(f'{{"schema":1,"root":{root},"tracked":['.encode())
        for values, suffix in ((tracked, b'],"groups":['), (groups, b"]}\n")):
            first = True
            for value in values:
                check()
                if not self._pid(value):
                    raise ObserverError("observer tracked/group values must be positive u32 PIDs")
                if not first:
                    request.append(44)
                request.extend(str(value).encode())
                first = False
                if len(request) > _MAX_REQUEST:
                    raise ObserverError("observer request exceeds 1 MiB")
            request.extend(suffix)
            if len(request) > _MAX_REQUEST:
                raise ObserverError("observer request exceeds 1 MiB")
        check()
        return request

    def _start(self) -> None:
        if not self._command:
            raise ObserverError("observer command is empty")
        # Unbuffered pipes are made nonblocking before any protocol traffic.
        # In particular, inherit the supervisor's session and process group.
        self._process = subprocess.Popen(self._command, stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, bufsize=0)
        for stream in (self._process.stdin, self._process.stdout, self._process.stderr):
            os.set_blocking(stream.fileno(), False)
        self._selector = selectors.DefaultSelector()
        for stream, name in ((self._process.stdout, "stdout"), (self._process.stderr, "stderr")):
            self._selector.register(stream, selectors.EVENT_READ, name)

    def _stderr_chunk(self) -> None:
        try:
            chunk = os.read(self._process.stderr.fileno(), _MAX_STDERR+1)
        except BlockingIOError:
            return
        if not chunk:
            if self._selector is not None:
                try:
                    self._selector.unregister(self._process.stderr)
                except KeyError:
                    pass
            return
        available = _MAX_STDERR-len(self._stderr)
        self._stderr.extend(chunk[:available])
        if len(chunk) > available:
            raise ObserverError("observer stderr exceeds 64 KiB")

    def _failure_diagnostics(self) -> None:
        # At most 64 KiB is retained, even if the helper floods its stderr.
        # This is nonblocking and bounded; never wait for a diagnostic writer.
        if self._process is None or self._process.stderr.closed or len(self._stderr) == _MAX_STDERR:
            return
        try:
            os.set_blocking(self._process.stderr.fileno(), False)
            self._stderr_chunk()
        except (OSError, ValueError, ObserverError):
            pass

    def _kill(self) -> None:
        if self._process is not None:
            self._process.kill()

    def _unsolicited_stdout(self) -> None:
        try:
            chunk = os.read(self._process.stdout.fileno(), 65536)
        except BlockingIOError:
            return
        if chunk:
            raise ObserverError("observer emitted an extra or unsolicited response frame")
        raise ObserverError("observer stdout reached EOF")

    def _validate(self, frame: bytearray, root: int, check: Callable[[], float]) -> dict:
        def unique_object(pairs):
            result = {}
            for key, value in pairs:
                check()
                if key in result:
                    raise ObserverError(f"duplicate observer response field: {key}")
                result[key] = value
            return result
        check()
        try:
            response = json.loads(frame, object_pairs_hook=unique_object)
        except (ValueError, TypeError, RecursionError) as error:
            raise ObserverError(f"malformed observer JSON response: {error}") from error
        check()
        if type(response) is not dict or set(response) != {"schema", "observer_pid", "processes"}:
            raise ObserverError("observer response must contain exactly schema, observer_pid and processes")
        if type(response["schema"]) is not int or response["schema"] != 1:
            raise ObserverError("unsupported observer response schema")
        if not self._pid(response["observer_pid"]) or response["observer_pid"] != self.pid:
            raise ObserverError("observer response PID does not match the spawned helper")
        rows = response["processes"]
        if type(rows) is not list:
            raise ObserverError("observer processes must be a list")
        seen = set()
        root_live = False
        observer_live = False
        for row in rows:
            check()
            if type(row) is not list or len(row) != 6:
                raise ObserverError("observer process row must contain exactly six fields")
            pid, ppid, pgid, rss, birth, state = row
            if not all(self._pid(value) for value in (pid, ppid, pgid)):
                raise ObserverError("observer process PIDs must be positive u32 integers")
            if pid in seen:
                raise ObserverError(f"duplicate observer process PID: {pid}")
            seen.add(pid)
            if type(rss) is not int or rss < 0:
                raise ObserverError("observer RSS must be a nonnegative integer")
            if type(birth) is not str or not birth or type(state) is not str or not state:
                raise ObserverError("observer birth and state must be nonempty strings")
            if pid == root:
                root_live = not state.startswith(("Z", "X", "x"))
            if pid == self.pid:
                observer_live = not state.startswith(("Z", "X", "x"))
        if not root_live:
            raise ObserverError("observer response is missing the requested live root")
        if not observer_live:
            raise ObserverError("observer response is missing its own live PID row")
        return response

    def observe(self, root: int, tracked: Iterable[int], groups: Iterable[int], *,
            deadline: float, cancellable: bool = True,
            interrupted: Callable[[], object] | None = None) -> dict:
        if self._closed:
            raise ObserverError("process observer is closed", self.stderr)
        if self._failed:
            raise ObserverError("process observer unavailable after previous failure", self.stderr)
        def check():
            return self._check(deadline, cancellable, interrupted)
        try:
            check()
            request = self._request(root, tracked, groups, check)
            if self._process is None:
                self._start()
            check()
            if self._process.poll() is not None:
                raise ObserverError(f"observer exited with status {self._process.returncode}")
            # No bytes left over from an earlier request may become this sample.
            self._unsolicited_stdout()
            self._stderr_chunk()
            self._selector.register(self._process.stdin, selectors.EVENT_WRITE, "stdin")
            offset = 0
            frame = bytearray()
            complete = False
            while not complete:
                remaining = check()
                if self._process.poll() is not None:
                    raise ObserverError(f"observer exited with status {self._process.returncode}")
                for key, _ in self._selector.select(min(_POLL_SECONDS, remaining)):
                    check()
                    if key.data == "stderr":
                        self._stderr_chunk()
                    elif key.data == "stdin":
                        try:
                            written = os.write(key.fd, memoryview(request)[offset:offset+65536])
                        except BlockingIOError:
                            continue
                        if written <= 0:
                            raise ObserverError("observer stdin made no progress")
                        offset += written
                        if offset == len(request):
                            self._selector.unregister(self._process.stdin)
                    else:
                        try:
                            chunk = os.read(key.fd, min(65536, _MAX_RESPONSE+1-len(frame)))
                        except BlockingIOError:
                            continue
                        if not chunk:
                            raise ObserverError("observer stdout reached EOF before a complete response")
                        previous = len(frame)
                        frame.extend(chunk)
                        if len(frame) > _MAX_RESPONSE:
                            raise ObserverError("observer response exceeds 16 MiB")
                        newline = frame.find(b"\n", previous)
                        if newline >= 0:
                            if newline != len(frame)-1:
                                raise ObserverError("observer emitted an extra response frame")
                            if offset != len(request):
                                raise ObserverError("observer responded before receiving the complete request")
                            complete = True
            response = self._validate(frame, root, check)
            self._stderr_chunk()
            self._unsolicited_stdout()
            check()
            if self._process.poll() is not None:
                raise ObserverError(f"observer exited with status {self._process.returncode}")
            return response
        except BaseException as error:
            self._failed = True
            self._failure_diagnostics()
            try:
                self._kill()
            except OSError as kill_error:
                raise ObserverError(f"{error}; cannot kill observer: {kill_error}", self.stderr) from error
            if isinstance(error, ObserverError):
                raise ObserverError(error.reason, self.stderr) from error
            if isinstance(error, (OSError, ValueError, subprocess.SubprocessError)):
                raise ObserverError(f"observer I/O failed: {error}", self.stderr) from error
            raise

    def close(self, *, deadline: float) -> None:
        """Kill and reap only this helper, without manufacturing shutdown time."""
        self._closed = True
        if self._process is None:
            return
        errors = []
        try:
            self._kill()
        except OSError as error:
            errors.append(f"cannot kill observer: {error}")
        self._failure_diagnostics()
        if self._selector is not None:
            self._selector.close()
        for stream in (self._process.stdin, self._process.stdout, self._process.stderr):
            if stream is not None and not stream.closed:
                try:
                    stream.close()
                except OSError as error:
                    errors.append(f"cannot close observer pipe: {error}")
        try:
            # Reap promptly after SIGKILL instead of charging a full sampling
            # interval to each helper's shutdown. Keep the original deadline.
            self._process.wait(timeout=max(0, deadline-time.monotonic()))
        except subprocess.TimeoutExpired:
            errors.append("observer cleanup deadline exceeded before helper was reaped")
        if time.monotonic() > deadline and not errors:
            errors.append("observer cleanup exceeded the original deadline")
        if errors:
            raise ObserverError("; ".join(errors), self.stderr)
