#!/usr/bin/env python3
"""Real-process tests for the persistent, deadline-bound observer transport."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

from semantic_observer import ObserverError, ProcessObserver


def process_rows(root, tracked, groups):
    """Independent fixture metadata, including real births, groups and RSS."""
    rows = {}
    if sys.platform == "linux":
        for entry in Path("/proc").glob("[0-9]*/stat"):
            try:
                text = entry.read_text()
                tail = text.rsplit(")", 1)[1].split()
                pid = int(entry.parent.name)
                rows[pid] = [pid, int(tail[1]), int(tail[2]),
                    int(tail[21])*os.sysconf("SC_PAGE_SIZE"), tail[19], tail[0]]
            except (FileNotFoundError, ProcessLookupError):
                continue
    else:
        output = subprocess.check_output(["/bin/ps", "-axo",
            "pid=,ppid=,pgid=,rss=,lstart=,stat="], text=True)
        for line in output.splitlines():
            fields = line.split()
            pid, ppid, pgid, rss = map(int, fields[:4])
            rows[pid] = [pid, ppid, pgid, rss*1024,
                " ".join(fields[4:-1]), fields[-1]]
    selected = {root, os.getpid(), *tracked}
    selected.update(pid for pid, row in rows.items() if row[2] in groups)
    while True:
        children = {pid for pid, row in rows.items() if row[1] in selected}
        if children <= selected:
            break
        selected.update(children)
    return [rows[pid] for pid in sorted(selected) if pid in rows]


def fixture(mode, directory, mutation=""):
    """Executed in a real helper, never substituted for Popen or pipe calls."""
    directory = Path(directory)
    with (directory/"starts").open("a") as output:
        output.write(f"{os.getpid()}\n")
    if mode == "workload":
        memory = bytearray(4*1024*1024)
        for offset in range(0, len(memory), 4096):
            memory[offset] = 7
        (directory/"ready").write_text(str(os.getpid()))
        time.sleep(60)
        return
    if mode == "startup-stall":
        os.write(2, b"fixture waiting during observer initialization\n")
        time.sleep(60)
        return
    for request_number, line in enumerate(sys.stdin.buffer, 1):
        request = json.loads(line)
        if mode in {"blocked-output", "late-output"} and request_number == 2:
            os.write(2, b"fixture received request but withholds response\n")
            time.sleep(.5)
            (directory/"late-write").write_text("observer survived")
            time.sleep(60)
            return
        if mode == "crash" or (mode == "crash-second" and request_number == 2):
            os.write(2, b"kernel metadata fixture failed\n")
            os._exit(23)
        if mode == "truncated":
            os.write(2, b"truncated native observation\n")
            os.write(1, b'{"schema":1,"observer_pid":')
            os._exit(24)
        if mode == "oversized":
            os.write(2, b"oversized native observation\n")
            for _ in range(258):
                os.write(1, b"x"*65536)
            time.sleep(60)
            return
        if mode == "stderr-flood":
            os.write(2, b"diagnostic-prefix\n")
            for _ in range(8):
                os.write(2, b"d"*16384)
            time.sleep(60)
            return
        response = {"schema": 1, "observer_pid": os.getpid(),
            "processes": process_rows(request["root"], request["tracked"], request["groups"])}
        if mode == "malformed":
            rows = response["processes"]
            root_row = next(row for row in rows if row[0] == request["root"])
            if mutation == "json":
                os.write(1, b"{not-json}\n")
                continue
            if mutation == "duplicate-key":
                payload = json.dumps(response).replace('"schema": 1', '"schema": 1, "schema": 1')
                os.write(1, payload.encode()+b"\n")
                continue
            if mutation == "schema":
                response["schema"] = True
            elif mutation == "observer-pid":
                response["observer_pid"] = os.getpid()+100000
            elif mutation == "observer-bool":
                response["observer_pid"] = True
            elif mutation == "missing-root":
                rows.remove(root_row)
            elif mutation == "missing-observer":
                response["processes"] = [row for row in rows if row[0] != os.getpid()]
            elif mutation == "duplicate-row":
                rows.append(root_row[:])
            elif mutation == "pid-bool":
                root_row[0] = True
            elif mutation == "ppid-zero":
                root_row[1] = 0
            elif mutation == "pgid-string":
                root_row[2] = str(root_row[2])
            elif mutation == "rss-negative":
                root_row[3] = -1
            elif mutation == "rss-bool":
                root_row[3] = False
            elif mutation == "short-row":
                root_row.pop()
            elif mutation == "birth-empty":
                root_row[4] = ""
            elif mutation == "state-empty":
                root_row[5] = ""
            elif mutation == "root-zombie":
                root_row[5] = "Z"
            elif mutation == "root-exiting":
                root_row[5] = "X"
            elif mutation == "unknown-field":
                response["unused"] = 1
            elif mutation == "missing-field":
                del response["processes"]
            elif mutation == "processes-object":
                response["processes"] = {}
            elif mutation == "noninteger-pid":
                root_row[0] = float(root_row[0])
            elif mutation != "extra-frame":
                raise AssertionError(mutation)
        encoded = json.dumps(response, separators=(",", ":")).encode()+b"\n"
        if mode == "partial":
            os.write(2, b"partial response diagnostic\n")
            for start in range(0, len(encoded), 11):
                os.write(1, encoded[start:start+11])
                time.sleep(.001)
        else:
            if mutation == "extra-frame":
                encoded += encoded
            os.write(1, encoded)
        if mode == "unsolicited":
            while not (directory/"send-extra").exists():
                time.sleep(.005)
            os.write(1, encoded)
            (directory/"extra-ready").write_text("extra response emitted")
        if mode == "blocked-input":
            os.write(2, b"fixture refuses stdin\n")
            time.sleep(.5)
            (directory/"late-write").write_text("observer survived")
            time.sleep(60)
            return


class ProcessObserverTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.deadline = time.monotonic()+30
        self.observers = []
        self.addCleanup(self.cleanup_observers)

    def cleanup_observers(self):
        for observer in self.observers:
            observer.close(deadline=self.deadline)

    def observer(self, mode="normal", mutation=""):
        directory = self.directory/f"helper-{len(self.observers)}"
        directory.mkdir()
        observer = ProcessObserver([sys.executable, str(Path(__file__).resolve()),
            "--fixture", mode, str(directory), mutation])
        self.observers.append(observer)
        return observer, directory

    def observe(self, observer, **kwargs):
        return observer.observe(os.getpid(), (), (), deadline=self.deadline, **kwargs)

    def assert_reaped(self, observer):
        self.assertIsNotNone(observer.pid)
        with self.assertRaises(ChildProcessError):
            os.waitpid(observer.pid, os.WNOHANG)
        with self.assertRaises(ProcessLookupError):
            os.kill(observer.pid, 0)

    def workload(self, *, isolated_group=False):
        directory = self.directory/f"workload-{time.monotonic_ns()}"
        directory.mkdir()
        process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()),
            "--fixture", "workload", str(directory)], stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
            process_group=0 if isolated_group else None)
        def cleanup():
            if process.poll() is None:
                process.kill()
            process.wait(timeout=max(.001, self.deadline-time.monotonic()))
            process.stderr.close()
        self.addCleanup(cleanup)
        while not (directory/"ready").exists():
            if process.poll() is not None or time.monotonic() >= self.deadline:
                self.fail("real memory workload did not become ready")
            time.sleep(.005)
        return process

    def test_lazy_helper_is_reused_across_real_descendant_replacement(self):
        observer, directory = self.observer()
        self.assertIsNone(observer.pid)
        self.assertFalse((directory/"starts").exists())
        first = self.workload()
        response = self.observe(observer)
        helper_pid = response["observer_pid"]
        before = {row[0]: row for row in response["processes"]}
        self.assertEqual(before[first.pid][1], os.getpid())
        self.assertEqual(before[first.pid][2], os.getpgid(first.pid))
        self.assertGreaterEqual(before[first.pid][3], 4*1024*1024)
        self.assertGreater(before[helper_pid][3], 0)
        self.assertEqual(os.getpgid(helper_pid), os.getpgrp())
        first.kill()
        first.wait(timeout=2)
        second = self.workload()
        response = observer.observe(os.getpid(), [first.pid, second.pid], [], deadline=self.deadline)
        after = {row[0]: row for row in response["processes"]}
        self.assertEqual(response["observer_pid"], helper_pid)
        self.assertNotIn(first.pid, after)
        self.assertEqual(after[second.pid][1], os.getpid())
        self.assertGreaterEqual(after[second.pid][3], 4*1024*1024)
        self.assertEqual(after[os.getpid()][4], before[os.getpid()][4])
        self.assertEqual(after[helper_pid][4], before[helper_pid][4])
        self.assertEqual((directory/"starts").read_text(), f"{helper_pid}\n")
        observer.close(deadline=self.deadline)
        self.assert_reaped(observer)

    def test_tracked_pids_and_groups_include_real_processes_outside_the_root_tree(self):
        observer, _ = self.observer()
        root = self.workload()
        target = self.workload(isolated_group=True)
        baseline = observer.observe(root.pid, [], [], deadline=self.deadline)
        self.assertNotIn(target.pid, {row[0] for row in baseline["processes"]})
        tracked = observer.observe(root.pid, [target.pid], [], deadline=self.deadline)
        tracked_rows = {row[0]: row for row in tracked["processes"]}
        self.assertEqual(tracked_rows[target.pid][1], os.getpid())
        self.assertGreaterEqual(tracked_rows[target.pid][3], 4*1024*1024)
        grouped = observer.observe(root.pid, [], [target.pid], deadline=self.deadline)
        grouped_rows = {row[0]: row for row in grouped["processes"]}
        self.assertEqual(grouped_rows[target.pid][2], target.pid)
        self.assertEqual(grouped_rows[target.pid][4], tracked_rows[target.pid][4])
        self.assertEqual(grouped["observer_pid"], baseline["observer_pid"])

    def test_partial_frames_reconstruct_real_metadata_and_keep_diagnostics(self):
        observer, _ = self.observer("partial")
        rows = {row[0]: row for row in self.observe(observer)["processes"]}
        self.assertEqual(rows[os.getpid()][1], os.getppid())
        self.assertEqual(rows[os.getpid()][2], os.getpgrp())
        self.assertGreater(rows[os.getpid()][3], 0)
        self.assertIn("partial response diagnostic", observer.stderr)
        self.assertEqual(self.observe(observer)["observer_pid"], observer.pid)

    def test_delayed_extra_frame_cannot_be_reused_as_the_next_observation(self):
        observer, directory = self.observer("unsolicited")
        first = self.observe(observer)
        (directory/"send-extra").write_text("emit an unsolicited response")
        while not (directory/"extra-ready").exists():
            if time.monotonic() >= self.deadline:
                self.fail("fixture did not emit its delayed extra response")
            time.sleep(.005)
        with self.assertRaisesRegex(ObserverError, "unsolicited"):
            self.observe(observer)
        observer.close(deadline=self.deadline)
        self.assertEqual(observer.pid, first["observer_pid"])
        self.assert_reaped(observer)

    def test_invalid_frames_fail_closed_and_never_restart(self):
        mutations = ("json", "duplicate-key", "schema", "observer-pid", "observer-bool",
            "missing-root", "missing-observer", "duplicate-row", "pid-bool", "ppid-zero",
            "pgid-string", "rss-negative", "rss-bool", "short-row", "birth-empty",
            "state-empty", "root-zombie", "root-exiting", "unknown-field", "missing-field",
            "processes-object", "noninteger-pid", "extra-frame")
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                # Each independent helper scenario owns the same original budget.
                self.deadline = time.monotonic()+30
                observer, directory = self.observer("malformed", mutation)
                with self.assertRaises(ObserverError):
                    self.observe(observer)
                pid = observer.pid
                with self.assertRaises(ObserverError):
                    self.observe(observer)
                observer.close(deadline=self.deadline)
                self.assertEqual(observer.pid, pid)
                self.assertEqual((directory/"starts").read_text(), f"{pid}\n")
                self.assert_reaped(observer)

    def test_first_observation_charges_stalled_helper_initialization_to_its_deadline(self):
        observer, _ = self.observer("startup-stall")
        self.assertIsNone(observer.pid)
        started = time.monotonic()
        with self.assertRaisesRegex(ObserverError, "deadline"):
            observer.observe(os.getpid(), [], [], deadline=started+.6)
        self.assertLess(time.monotonic()-started, .9)
        self.assertIsNotNone(observer.pid)
        pid = observer.pid
        with self.assertRaises(ObserverError):
            self.observe(observer)
        self.assertEqual(observer.pid, pid)
        observer.close(deadline=self.deadline)
        self.assert_reaped(observer)

    def test_deadline_bounds_blocked_stdin_and_prevents_late_writers(self):
        observer, directory = self.observer("blocked-input")
        self.observe(observer)
        started = time.monotonic()
        with self.assertRaisesRegex(ObserverError, "deadline"):
            observer.observe(os.getpid(), range(1, 110000), [], deadline=started+.3)
        self.assertLess(time.monotonic()-started, .6)
        self.assertIn("fixture refuses stdin", observer.stderr)
        observer.close(deadline=self.deadline)
        self.assert_reaped(observer)
        time.sleep(.55)
        self.assertFalse((directory/"late-write").exists())

    def test_cancellation_bounds_blocked_stdout_and_prevents_late_writers(self):
        observer, directory = self.observer("late-output")
        self.observe(observer)
        cancel_at = time.monotonic()+.25
        with self.assertRaisesRegex(ObserverError, "interrupted.*SIGTERM"):
            self.observe(observer, interrupted=lambda: "SIGTERM" if time.monotonic() >= cancel_at else None)
        self.assertLess(time.monotonic()-cancel_at, .15)
        self.assertIn("withholds response", observer.stderr)
        observer.close(deadline=self.deadline)
        self.assert_reaped(observer)
        time.sleep(.55)
        self.assertFalse((directory/"late-write").exists())

    def test_noncancellable_cleanup_still_obeys_output_deadline(self):
        observer, _ = self.observer("blocked-output")
        self.observe(observer)
        def forbidden_signal_check():
            raise AssertionError("cleanup must ignore cancellation")
        started = time.monotonic()
        with self.assertRaisesRegex(ObserverError, "deadline"):
            observer.observe(os.getpid(), [], [], deadline=started+.3,
                cancellable=False, interrupted=forbidden_signal_check)
        self.assertLess(time.monotonic()-started, .6)
        self.assertIn("withholds response", observer.stderr)
        observer.close(deadline=self.deadline)
        self.assert_reaped(observer)

    def test_crash_and_truncation_preserve_diagnostics_and_reap(self):
        for mode, diagnostic in (("crash", "kernel metadata fixture failed"),
                ("truncated", "truncated native observation")):
            with self.subTest(mode=mode):
                observer, _ = self.observer(mode)
                with self.assertRaises(ObserverError) as raised:
                    self.observe(observer)
                self.assertIn(diagnostic, str(raised.exception))
                self.assertIn(diagnostic, observer.stderr)
                observer.close(deadline=self.deadline)
                self.assert_reaped(observer)

    def test_crash_on_later_request_does_not_restart_the_reused_helper(self):
        observer, directory = self.observer("crash-second")
        first = self.observe(observer)
        with self.assertRaisesRegex(ObserverError, "kernel metadata fixture failed"):
            self.observe(observer)
        with self.assertRaisesRegex(ObserverError, "previous failure"):
            self.observe(observer)
        observer.close(deadline=self.deadline)
        self.assertEqual((directory/"starts").read_text(), f"{first['observer_pid']}\n")
        self.assert_reaped(observer)

    def test_oversized_stdout_and_stderr_are_bounded_and_fail_closed(self):
        for mode, diagnostic in (("oversized", "oversized native observation"),
                ("stderr-flood", "diagnostic-prefix")):
            with self.subTest(mode=mode):
                observer, _ = self.observer(mode)
                with self.assertRaisesRegex(ObserverError, "exceeds"):
                    self.observe(observer)
                self.assertIn(diagnostic, observer.stderr)
                self.assertLessEqual(len(observer.stderr.encode()), 64*1024)
                observer.close(deadline=self.deadline)
                self.assert_reaped(observer)

    def test_oversized_request_poisoning_reaps_the_existing_helper(self):
        observer, directory = self.observer()
        self.observe(observer)
        pid = observer.pid
        with self.assertRaisesRegex(ObserverError, "request exceeds"):
            observer.observe(os.getpid(), range(1, 220000), [], deadline=self.deadline)
        with self.assertRaisesRegex(ObserverError, "previous failure"):
            self.observe(observer)
        observer.close(deadline=self.deadline)
        self.assertEqual((directory/"starts").read_text(), f"{pid}\n")
        self.assert_reaped(observer)

    def test_close_is_final_and_reaps_only_the_observer(self):
        observer, _ = self.observer()
        sibling = self.workload()
        self.observe(observer)
        observer.close(deadline=self.deadline)
        self.assert_reaped(observer)
        self.assertIsNone(sibling.poll())
        with self.assertRaisesRegex(ObserverError, "closed"):
            self.observe(observer)

    def test_close_with_exhausted_deadline_reports_cleanup_failure_without_extra_wait(self):
        observer, _ = self.observer()
        self.observe(observer)
        started = time.monotonic()
        with self.assertRaisesRegex(ObserverError, "cleanup.*deadline"):
            observer.close(deadline=started-1)
        self.assertLess(time.monotonic()-started, .1)
        # The same original test deadline is still available for the fixture's
        # final reap; close itself must not create a replacement time budget.
        observer.close(deadline=self.deadline)
        self.assert_reaped(observer)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--fixture":
        fixture(*sys.argv[2:])
    else:
        unittest.main()
