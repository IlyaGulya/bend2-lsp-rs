"""Process-group signaling, including macOS's unreaped zombie-only EPERM."""
import os
import subprocess
import time


def signal_group(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        return
    except PermissionError as failure:
        deadline = time.monotonic() + .1
        # Bounded wait reaps an exited leader, including concurrent reader/close
        # races where Popen.poll() would report None while its wait lock is held.
        # EPERM itself never establishes that descendants are absent.
        try:
            process.wait(timeout=.1)
        except subprocess.TimeoutExpired:
            raise failure
        while True:
            try:
                os.killpg(process.pid, sig)
                return
            except ProcessLookupError:
                return
            except PermissionError:
                # Reaping the leader does not reap its orphaned descendants.
                # Keep the same bounded grace until the kernel reports absence;
                # never treat an inaccessible live group as successful cleanup.
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise
                time.sleep(min(.005, remaining))
