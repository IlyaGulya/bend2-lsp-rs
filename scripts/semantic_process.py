"""Process-group signaling, including macOS's unreaped zombie-only EPERM."""
import os
import subprocess


def signal_group(process, sig):
    try:
        os.killpg(process.pid, sig)
    except ProcessLookupError:
        return
    except PermissionError as failure:
        # Bounded wait reaps an exited leader, including concurrent reader/close
        # races where Popen.poll() would report None while its wait lock is held.
        # EPERM itself never establishes that descendants are absent.
        try:
            process.wait(timeout=.1)
        except subprocess.TimeoutExpired:
            raise failure
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            return
        # A surviving inaccessible group still raises PermissionError. A live
        # accessible group receives the requested signal, including escalation.
