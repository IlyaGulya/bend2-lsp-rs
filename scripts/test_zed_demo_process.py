#!/usr/bin/env python3
"""Real process-lifecycle regressions for the optional POSIX recorder/build."""

import os
import selectors
import signal
import subprocess
import sys
import unittest

from zed_demo_process import owned_process


CHILD_WITH_DESCENDANT = """
import signal
import subprocess
import sys

child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])

def shutdown(signum, frame):
    child.wait(timeout=5)
    raise SystemExit(0)

signal.signal(signal.SIGTERM, shutdown)
print(child.pid, flush=True)
signal.pause()
"""


@unittest.skipUnless(os.name == "posix", "the windowless recorder uses POSIX process groups")
class OwnedProcessTests(unittest.TestCase):
    def test_interrupt_stops_the_owned_process_and_its_descendant(self):
        process = None
        try:
            with self.assertRaises(KeyboardInterrupt):
                with owned_process(
                    [sys.executable, "-S", "-c", CHILD_WITH_DESCENDANT],
                    stdout=subprocess.PIPE,
                    text=True,
                ) as process:
                    with selectors.DefaultSelector() as selector:
                        selector.register(process.stdout, selectors.EVENT_READ)
                        if not selector.select(timeout=10):
                            raise RuntimeError("controlled child did not become ready")
                    descendant = int(process.stdout.readline())
                    raise KeyboardInterrupt

            self.assertIsNotNone(process.returncode)
            with self.assertRaises(ProcessLookupError):
                os.kill(descendant, 0)
        finally:
            if process is not None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=10)


if __name__ == "__main__":
    unittest.main()
