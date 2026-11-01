#!/usr/bin/env python3
"""Bound finite recorder/build subprocesses to their owning POSIX scope."""

from contextlib import contextmanager
import os
import signal
import subprocess


def _stop_group(process):
    # The primary process may already have exited while descendants remain.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        process.wait()
        return
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


@contextmanager
def owned_process(command, **options):
    """Stop and reap this command's private process group on every scope exit."""
    if os.name != "posix":
        raise ValueError("the optional recorder/build requires POSIX process groups")
    process = subprocess.Popen(command, start_new_session=True, **options)
    try:
        yield process
    finally:
        try:
            _stop_group(process)
        finally:
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None:
                    try:
                        stream.close()
                    except BrokenPipeError:
                        pass
