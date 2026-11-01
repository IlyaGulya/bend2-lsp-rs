#!/usr/bin/env python3
"""Native-contract transport over a real bounded OS pipe.

Bun/Node spawn stdio "pipe" is a Unix socketpair in the pinned runtime, which
can buffer whole replies and hide stdout loss. This harness owns an os.pipe(),
bounds it to 8192 bytes where the platform supports F_SETPIPE_SZ, and runs the
authenticated worker with that pipe as its stdout. Captured frames and transport
facts are written for the native contract to assert on; the worker stays alive
across both requests with stdin open.
"""
import json
import os
import queue
import signal
import subprocess
import sys
import time
import threading

try:
    import fcntl
except ImportError:  # Non-POSIX platforms keep default pipe behavior.
    fcntl = None

FRAME_SECONDS = 10.0
EXIT_SECONDS = 5.0
PIPE_BYTES = 8192


class FrameFailure(RuntimeError):
    def __init__(self, message, data=b'', chunks=None):
        super().__init__(message)
        self.data = data
        self.chunks = chunks or []


def pipe_capacity(fd):
    try:
        return fcntl.fcntl(fd, fcntl.F_GETPIPE_SZ)
    except (AttributeError, OSError):
        return None


def bound_pipe(fd):
    try:
        fcntl.fcntl(fd, fcntl.F_SETPIPE_SZ, PIPE_BYTES)
    except (AttributeError, OSError):
        pass
    return pipe_capacity(fd)


def read_pipe(fd, events):
    try:
        while True:
            chunk = os.read(fd, 65536)
            events.put(chunk)
            if not chunk:
                return
    except OSError as error:
        events.put(error)


def next_chunk(events, deadline, data, chunks):
    remaining = deadline - time.monotonic()
    try:
        chunk = events.get(timeout=max(0, remaining))
    except queue.Empty as error:
        raise FrameFailure('worker-reply-timeout:' + str(len(data)), bytes(data), chunks) from error
    if isinstance(chunk, OSError):
        raise chunk
    return chunk


def read_frame(events, timeout):
    deadline = time.monotonic() + timeout
    data = bytearray()
    chunks = []
    while True:
        chunk = next_chunk(events, deadline, data, chunks)
        if not chunk:
            raise FrameFailure('worker-reply-eof:' + str(len(data)), bytes(data), chunks)
        chunks.append(len(chunk))
        data.extend(chunk)
        newline = data.find(b'\n')
        if newline >= 0:
            return bytes(data[:newline]), len(data) - newline - 1, chunks, len(data)


def main():
    if len(sys.argv) != 6:
        print('usage: semantic_frame_pipe.py RUNTIME WORKER REQUEST0 REQUEST1 OUTDIR', file=sys.stderr)
        return 2
    runtime, worker, request0_path, request1_path, outdir = sys.argv[1:]
    os.makedirs(outdir, exist_ok=True)
    home = os.path.join(outdir, 'home')
    os.makedirs(home, exist_ok=True)
    requests = []
    for request_path in (request0_path, request1_path):
        payload = open(request_path, 'rb').read().strip()
        json.loads(payload)
        requests.append(payload)
    transport = {'schema': 1, 'status': 'fail', 'error': None, 'capacity': None,
                 'fd1': None, 'frames': [], 'exit': None, 'stderr': ''}
    process = None
    read_fd = write_fd = None
    wrote = set()
    index = -1
    try:
        read_fd, write_fd = os.pipe()
        transport['capacity'] = bound_pipe(write_fd)
        if sys.platform.startswith('linux') and (transport['capacity'] is None or
                                                 transport['capacity'] > 2 * PIPE_BYTES):
            raise RuntimeError('worker-pipe-capacity:' + str(transport['capacity']))
        environment = {**os.environ, 'HOME': home, 'BEND_LIB': os.path.join(home, 'lib'),
                       'BEND_HUB': 'http://127.0.0.1:1'}
        process = subprocess.Popen([runtime, worker], stdin=subprocess.PIPE, stdout=write_fd,
                                   stderr=subprocess.PIPE, env=environment, cwd=home,
                                   start_new_session=os.name != 'nt')
        os.close(write_fd)
        write_fd = None
        events = queue.Queue(maxsize=2)
        threading.Thread(target=read_pipe, args=(read_fd, events), daemon=True).start()
        try:
            transport['fd1'] = os.readlink(f'/proc/{process.pid}/fd/1')
        except OSError:
            pass
        for index, payload in enumerate(requests):
            process.stdin.write(payload + b'\n')
            process.stdin.flush()
            frame, trailing, chunks, total = read_frame(events, FRAME_SECONDS)
            open(os.path.join(outdir, f'frame-{index}.bin'), 'wb').write(frame)
            wrote.add(index)
            transport['frames'].append({'bytes': len(frame), 'newline': True,
                                        'trailing': trailing, 'chunks': chunks, 'read': total})
            if trailing:
                raise FrameFailure('worker-multiple-replies:' + str(trailing), frame, chunks)
            if transport['capacity'] is not None and len(frame) <= transport['capacity']:
                raise FrameFailure('worker-frame-within-pipe-capacity:' + str(len(frame)) +
                                   '<=' + str(transport['capacity']), frame, chunks)
        process.stdin.close()
        exit_deadline = time.monotonic() + EXIT_SECONDS
        transport['exit'] = process.wait(timeout=EXIT_SECONDS)
        if transport['exit'] != 0:
            raise RuntimeError('worker-exit:' + str(transport['exit']))
        extra = next_chunk(events, exit_deadline, b'', [])
        if extra:
            raise FrameFailure('worker-extra-reply:' + str(len(extra)), extra, [len(extra)])
        if transport['fd1'] is not None and not transport['fd1'].startswith('pipe:'):
            raise RuntimeError('worker-stdout-not-os-pipe:' + transport['fd1'])
        transport['status'] = 'pass'
        return 0
    except (FrameFailure, OSError, RuntimeError, subprocess.SubprocessError, ValueError) as error:
        transport['error'] = str(error)
        data = getattr(error, 'data', b'')
        chunks = getattr(error, 'chunks', [])
        if 0 <= index < len(requests) and index not in wrote:
            open(os.path.join(outdir, f'frame-{index}.bin'), 'wb').write(data)
            transport['frames'].append({'bytes': len(data), 'newline': False,
                                        'trailing': 0, 'chunks': chunks, 'read': len(data)})
        return 1
    finally:
        if process is not None:
            try:
                if process.stdin is not None and not process.stdin.closed:
                    process.stdin.close()
            except OSError:
                pass
            if process.poll() is None:
                if os.name == 'nt':
                    process.kill()
                else:
                    try:
                        os.killpg(process.pid, signal.SIGKILL)
                    except OSError:
                        process.kill()
                try:
                    process.wait(timeout=1)
                except subprocess.TimeoutExpired:
                    pass
            try:
                transport['stderr'] = process.stderr.read().decode(errors='replace')[:8192]
            except (OSError, ValueError):
                pass
        if read_fd is not None:
            os.close(read_fd)
        if write_fd is not None:
            os.close(write_fd)
        with open(os.path.join(outdir, 'transport.json'), 'w') as stream:
            stream.write(json.dumps(transport, sort_keys=True) + '\n')
        if transport['status'] == 'pass':
            print(json.dumps({'status': 'pass', 'capacity': transport['capacity'],
                              'frames': [frame['bytes'] for frame in transport['frames']]}))
        else:
            print(transport['error'], file=sys.stderr)


if __name__ == '__main__':
    sys.exit(main())
