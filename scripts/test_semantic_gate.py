#!/usr/bin/env python3
"""Behavioral supervisor tests; no compiler/oracle is spawned per unit test."""
import copy
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request
import zipfile
from unittest.mock import patch

import semantic_gate as gate
import bootstrap_bend
import semantic_install as installer


@unittest.skipUnless(sys.platform in {"linux", "darwin"}, "explicit supported RSS platforms")
class SupervisorTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.limits = {**gate.PROFILE, "shutdown_seconds": .3, "sample_seconds": .01,
            "rss_bytes": 512*1024*1024, "output_bytes": 4096}

    def run_child(self, code, seconds=2, **changes):
        limits = {**self.limits, **changes}
        return gate.supervise([sys.executable, "-c", code],
            deadline=time.monotonic()+seconds, output_path=self.directory/"output.log",
            artifact_dir=self.directory, limits=limits)

    def delay_first_ps(self, seconds, observers):
        # Exec the real observer with the same PID after a scheduling delay.
        # Only its launch is controlled; RSS and lifecycle evidence are real.
        spawn = subprocess.Popen
        def delayed(command, *args, **kwargs):
            if command[0] in {"ps", "/bin/ps"} and not observers:
                command = [sys.executable, "-c",
                    f"import os,sys,time; time.sleep({seconds!r}); os.execv('/bin/ps',['ps']+sys.argv[1:])",
                    *command[1:]]
                observer = spawn(command, *args, **kwargs)
                observers.append(observer)
                return observer
            return spawn(command, *args, **kwargs)
        return patch.object(gate.subprocess, "Popen", side_effect=delayed)

    def assert_dead(self, pid):
        table = gate.process_table()
        # Lifecycle is independent of RSS: Darwin zombies can retain pages,
        # while a live process with zero sampled RSS must still fail this check.
        self.assertTrue(pid not in table or not gate.is_live_process(table[pid]), f"live child remains: {pid}")

    def test_success_has_real_memory_and_output(self):
        result = self.run_child("print('complete')")
        self.assertEqual(result["status"], "pass", result)
        self.assertGreater(result["peak_process_tree_rss_bytes"], 0)
        self.assertGreater(result["rss_samples"], 0)
        self.assertEqual((self.directory/"output.log").read_text(), "complete\n")

    def test_monitor_does_not_register_itself_as_a_workload_child(self):
        existing = set(gate.process_table())
        tree = gate.ProcessTree()
        for _ in range(3):
            tree.sample()
        self.assertFalse(tree.children-existing, f"observer PIDs tracked as workload: {tree.children-existing}")
        snapshot = gate.process_table()
        if sys.platform=="darwin":
            self.assertGreater(snapshot.observer_rss_bytes,0)
        else:
            self.assertEqual(snapshot.observer_rss_bytes,0)

    @unittest.skipUnless(sys.platform=="linux", "Linux proc stat accounting")
    def test_proc_command_parentheses_preserve_identity_and_rss_units(self):
        pid = os.getpid()
        command = Path("/proc/self/comm")
        original = command.read_text()
        before = gate.process_table()[pid]
        try:
            command.write_text("rss) space (\n")
            observed = gate.process_table()[pid]
            self.assertEqual(observed[0], os.getppid())
            self.assertEqual(observed[1], os.getpgrp())
            self.assertEqual(observed[3], before[3])
            self.assertTrue(gate.is_live_process(observed))
            self.assertGreater(observed[2], 0)
            self.assertEqual(observed[2] % os.sysconf("SC_PAGE_SIZE"), 0)
        finally:
            command.write_text(original)

    def test_real_child_named_ps_remains_observed(self):
        process = subprocess.Popen(["ps","-c","import time; time.sleep(60)"],
            executable=sys.executable, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            tree = gate.ProcessTree()
            _, table, owned = tree.sample()
            self.assertIn(process.pid,owned)
            self.assertIn(process.pid,tree.children)
            self.assertTrue(gate.is_live_process(table[process.pid]))
        finally:
            process.kill()
            process.wait(timeout=2)

    def test_zombie_only_group_is_not_live_and_direct_child_is_reaped(self):
        process = subprocess.Popen([sys.executable,"-c","pass"], start_new_session=True,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            deadline = time.monotonic()+2
            row = None
            while time.monotonic()<deadline:
                row = gate.process_table().get(process.pid)
                if row is not None and not gate.is_live_process(row):
                    break
                time.sleep(.01)
            self.assertIsNotNone(row)
            self.assertFalse(gate.is_live_process(row))
            tree = gate.ProcessTree()
            tree.sample()
            self.assertIn(process.pid,tree.children)
            tree.kill()
            self.assertEqual(process.wait(timeout=1),0)
        finally:
            if process.returncode is None:
                process.kill()
                process.wait(timeout=2)

    @unittest.skipUnless(sys.platform == "linux", "Linux child subreaping")
    def test_quiet_success_reaps_an_adopted_zombie(self):
        code = ("import os\n"
            "child=os.fork()\n"
            "if child==0: os._exit(0)\n"
            "os.waitid(os.P_PID,child,os.WEXITED|os.WNOWAIT)\n"
            "print(child,flush=True)\n")
        result = self.run_child(code)
        self.assertEqual(result["status"], "pass", result)
        child = int((self.directory/"output.log").read_text())
        with self.assertRaises(ChildProcessError):
            os.waitpid(child, os.WNOHANG)

    @unittest.skipUnless(sys.platform == "linux", "Linux child subreaping")
    def test_cleanup_reaps_a_child_exiting_after_the_first_nonblocking_wait(self):
        code = ("import os\n"
            "child=os.fork()\n"
            "if child==0: os._exit(0)\n"
            "os.waitid(os.P_PID,child,os.WEXITED|os.WNOWAIT)\n"
            "print(child,flush=True)\n")
        waitpid = os.waitpid
        first_scan = True
        def late_exit(pid, options):
            nonlocal first_scan
            if pid == -1 and options == os.WNOHANG and first_scan:
                # Control only the first poll's scheduling outcome. The real
                # adopted child must still be collected from the kernel.
                first_scan = False
                return (0, 0)
            return waitpid(pid, options)
        try:
            with patch.object(gate.os, "waitpid", side_effect=late_exit):
                result = self.run_child(code)
            self.assertEqual(result["status"], "pass", result)
            child = int((self.directory/"output.log").read_text())
            with self.assertRaises(ChildProcessError):
                os.waitpid(child, os.WNOHANG)
        finally:
            gate.ProcessTree().reap()

    def test_success_with_closed_child_stdout_still_terminates_the_child(self):
        pidfile, release = self.directory/"detached.pid", self.directory/"release"
        code = ("import pathlib,subprocess,sys,time\n"
            "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],"
            "stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True)\n"
            f"pathlib.Path({str(pidfile)!r}).write_text(str(child.pid))\n"
            f"while not pathlib.Path({str(release)!r}).exists(): time.sleep(.005)\n"
            "print('complete')\n")
        class ObservedChild(gate.ProcessTree):
            def sample(self):
                observation = super().sample()
                if pidfile.is_file():
                    child = pidfile.read_text()
                    if child and int(child) in observation[2]:
                        release.touch()
                return observation
        result = gate.supervise([sys.executable,"-c",code],
            deadline=time.monotonic()+10, output_path=self.directory/"output.log",
            artifact_dir=self.directory, tree=ObservedChild())
        self.assertEqual(result["status"], "pass", result)
        self.assertTrue(release.is_file(), "child was not observed before leader exit")
        self.assertEqual(result["returncode"], 0)
        self.assertFalse(result["cleanup_errors"])
        child = int(pidfile.read_text())
        self.assert_dead(child)
        if sys.platform == "linux":
            with self.assertRaises(ChildProcessError):
                os.waitpid(child, os.WNOHANG)

    def test_primary_output_failure_survives_cleanup_failure_and_worker_is_reaped(self):
        class FailedCleanup(gate.ProcessTree):
            def kill(self, *, quiescent=None):
                super().kill(quiescent=quiescent)
                raise RuntimeError("injected cleanup observation failure")
        code = "import os;"+chr(10)+"while True: os.write(1,b'x'*1024)"
        result = gate.supervise([sys.executable,"-c",code],
            deadline=time.monotonic()+2, output_path=self.directory/"output.log",
            artifact_dir=self.directory, limits=self.limits, tree=FailedCleanup())
        self.assertEqual(result["status"],"error")
        self.assertIn("output limit",result["error"])
        self.assertTrue(any("injected cleanup" in error for error in result["cleanup_errors"]))
        self.assertIsNotNone(result["returncode"])

    @unittest.skipUnless(sys.platform=="darwin", "macOS ps observer")
    def test_slow_valid_memory_sample_does_not_abort_work_with_budget_remaining(self):
        observers = []
        with self.delay_first_ps(.65, observers):
            result = self.run_child("print('complete')", seconds=3)
        self.assertEqual(result["status"], "pass", result)
        self.assertGreater(result["peak_process_tree_rss_bytes"], 0)
        self.assertGreaterEqual(result["max_rss_sample_seconds"], .5)
        self.assertEqual((self.directory/"output.log").read_text(), "complete\n")
        self.assertEqual(len(observers), 1)
        self.assertIsNotNone(observers[0].returncode)

    @unittest.skipUnless(sys.platform=="darwin", "macOS ps observer")
    def test_slow_sample_still_enforces_memory_budget(self):
        base = gate.ProcessTree().sample()[0]
        pidfile = self.directory/"slow-memory.pid"
        code = "import os,time; open("+repr(str(pidfile))+",'w').write(str(os.getpid())); x=bytearray(128*1024*1024); time.sleep(60)"
        observers = []
        with self.delay_first_ps(.65, observers):
            result = self.run_child(code, seconds=3, rss_bytes=base+48*1024*1024)
        self.assertEqual(result["status"], "error", result)
        self.assertIn("RSS", result["error"])
        self.assertGreater(result["peak_process_tree_rss_bytes"], base+48*1024*1024)
        self.assert_dead(int(pidfile.read_text()))
        self.assertIsNotNone(observers[0].returncode)

    @unittest.skipUnless(sys.platform=="darwin", "macOS ps observer")
    def test_stalled_observer_cannot_outlive_phase_or_pass_without_measurement(self):
        observers = []
        # Even a successful worker cannot pass without a completed observation.
        with self.delay_first_ps(60, observers):
            result = self.run_child("print('complete')", seconds=1)
        self.assertEqual(result["status"], "error", result)
        self.assertIn("deadline", result["error"])
        self.assertLess(result["elapsed_seconds"], 1.3)
        self.assertIsNotNone(result["returncode"])
        self.assertIsNotNone(observers[0].returncode)
        self.assert_dead(observers[0].pid)

    @unittest.skipUnless(sys.platform=="darwin", "macOS ps observer")
    def test_shutdown_signal_interrupts_stalled_observation_and_reaps_both_children(self):
        observers = []
        pidfile = self.directory/"slow-monitor.pid"
        code = "import os,time; open("+repr(str(pidfile))+",'w').write(str(os.getpid())); time.sleep(60)"
        signaled_at = []
        def interrupt_observation():
            ready_deadline = time.monotonic()+2
            while time.monotonic()<ready_deadline:
                if observers and pidfile.exists():
                    signaled_at.append(time.monotonic())
                    gate.handle_shutdown_signal(signal.SIGTERM, None)
                    return
                time.sleep(.005)
        interrupter = threading.Thread(target=interrupt_observation)
        with patch.object(gate, "INTERRUPTED", None), self.delay_first_ps(60, observers):
            interrupter.start()
            try:
                result = self.run_child(code, seconds=3)
            finally:
                interrupter.join()
        self.assertEqual(result["status"], "error", result)
        self.assertIn("SIGTERM", result["error"])
        self.assertTrue(signaled_at, "worker and observer did not become ready")
        self.assertLess(time.monotonic()-signaled_at[0], 1)
        self.assertIsNotNone(result["returncode"])
        self.assertIsNotNone(observers[0].returncode)
        self.assert_dead(observers[0].pid)
        self.assert_dead(int(pidfile.read_text()))

    def test_timeout_kills_grandchild_and_reaps_worker(self):
        pidfile = self.directory/"grandchild.pid"
        code = "import subprocess,sys,time; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); open(sys.argv[1],'w').write(str(p.pid)); time.sleep(60)"
        result = gate.supervise([sys.executable, "-c", code, str(pidfile)],
            deadline=time.monotonic()+.9, output_path=self.directory/"output.log",
            artifact_dir=self.directory, limits=self.limits)
        self.assertEqual(result["status"], "error")
        self.assertIn("deadline", result["error"])
        self.assertLess(result["elapsed_seconds"], 1.2)
        self.assert_dead(int(pidfile.read_text()))
        self.assertIsNotNone(result["returncode"])

    def test_timeout_kills_descendants_in_a_separate_session(self):
        pidfile = self.directory/"escaped.pid"
        child = "import os,subprocess,sys,time; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); open("+repr(str(pidfile))+",'w').write(str(os.getpid())+' '+str(p.pid)); time.sleep(60)"
        code = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-c',"+repr(child)+"],start_new_session=True); time.sleep(60)"
        result = self.run_child(code, seconds=1)
        self.assertEqual(result["status"], "error")
        for pid in pidfile.read_text().split():
            self.assert_dead(int(pid))

    def test_monitor_failure_still_kills_owned_group(self):
        pidfile = self.directory/"monitored.pid"
        class FailedMonitor(gate.ProcessTree):
            def sample(self):
                # Inject only after the real worker has published its PID.
                # A sample count races interpreter startup on a busy host.
                if pidfile.is_file() and pidfile.read_text():
                    raise RuntimeError("injected monitor unavailable")
                return super().sample()
        code = "import os,time; open("+repr(str(pidfile))+",'w').write(str(os.getpid())); time.sleep(60)"
        result = gate.supervise([sys.executable,"-c",code], deadline=time.monotonic()+2,
            output_path=self.directory/"output.log", artifact_dir=self.directory,
            limits=self.limits, tree=FailedMonitor())
        self.assertEqual(result["status"], "error")
        self.assertIn("injected monitor unavailable", result["error"])
        self.assert_dead(int(pidfile.read_text()))
        self.assertIsNotNone(result["returncode"])

    def test_child_memory_is_aggregated_and_killed(self):
        base = gate.ProcessTree().sample()[0]
        pidfile = self.directory/"memory.pid"
        child = "import os,time; open("+repr(str(pidfile))+",'w').write(str(os.getpid())); x=bytearray(128*1024*1024); time.sleep(60)"
        code = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-c',"+repr(child)+"]); time.sleep(60)"
        result = self.run_child(code, seconds=3, rss_bytes=base+48*1024*1024)
        self.assertEqual(result["status"], "error")
        self.assertIn("RSS", result["error"])
        self.assertGreater(result["peak_process_tree_rss_bytes"], base+48*1024*1024)
        self.assert_dead(int(pidfile.read_text()))

    def test_output_flood_is_bounded_and_kills_tree(self):
        result = self.run_child("import os;\nwhile True: os.write(1,b'x'*1024)")
        self.assertEqual(result["status"], "error")
        self.assertIn("output limit", result["error"])
        self.assertLessEqual((self.directory/"output.log").stat().st_size, 4096)
        self.assertIsNotNone(result["returncode"])

    def test_artifact_growth_fails_without_truncating_counterexample(self):
        witness = self.directory/"original.json"
        code = "import time; open("+repr(str(witness))+",'wb').write(b'x'*8192); time.sleep(60)"
        result = self.run_child(code, artifact_bytes=4096)
        self.assertEqual(result["status"], "error")
        self.assertIn("artifact limit", result["error"])
        self.assertEqual(witness.stat().st_size, 8192)

    def test_final_artifact_write_before_clean_exit_is_accounted(self):
        for mode in ("bytes", "count"):
            with self.subTest(mode=mode):
                artifact_dir = self.directory/mode
                artifact_dir.mkdir()
                witness = artifact_dir/"original.json"
                # No output after the initial byte: the final artifact write
                # occurs while the supervisor is waiting for EOF, then exits.
                writes = f"open({str(witness)!r},'wb').write(b'x'*8192)" if mode=="bytes" else (
                    f"open({str(witness)!r},'w').write('original'); "
                    f"[open({str(artifact_dir)!r}+'/'+str(i),'w').close() for i in range(8)]")
                code = "import os,time; os.write(1,b'x'); time.sleep(.1); "+writes
                limits = {**self.limits, "sample_seconds":.5,
                    "artifact_bytes":4096 if mode=="bytes" else 16384, "artifact_files":4}
                result = gate.supervise([sys.executable,"-c",code], deadline=time.monotonic()+2,
                    output_path=artifact_dir/"output.log", artifact_dir=artifact_dir, limits=limits)
                self.assertEqual(result["status"], "error")
                self.assertIn("artifact limit", result["error"])
                self.assertTrue(witness.exists())
                if mode=="bytes":
                    self.assertEqual(witness.stat().st_size,8192)

    def test_final_artifact_violation_survives_cleanup_observation_failure(self):
        class FailedCleanup(gate.ProcessTree):
            def kill(self, *, quiescent=None):
                super().kill(quiescent=quiescent)
                raise RuntimeError("injected cleanup observation failure")
        witness = self.directory/"original.json"
        code = "import os,time; os.write(1,b'x'); time.sleep(.1); open("+repr(str(witness))+",'wb').write(b'x'*8192)"
        result = gate.supervise([sys.executable,"-c",code],
            deadline=time.monotonic()+2, output_path=self.directory/"output.log",
            artifact_dir=self.directory, limits={**self.limits,"sample_seconds":.5,
                "artifact_bytes":4096}, tree=FailedCleanup())
        self.assertEqual(result["status"], "error")
        self.assertIn("artifact limit", result["error"])
        self.assertTrue(any("injected cleanup observation failure" in error
            for error in result["cleanup_errors"]))
        self.assertEqual(witness.stat().st_size,8192)
        self.assertIsNotNone(result["returncode"])

    def test_nonzero_worker_cannot_pass(self):
        result = self.run_child("raise SystemExit(7)")
        self.assertEqual(result["status"], "error")
        self.assertEqual(result["returncode"], 7)

    def test_missing_executable_fails_with_report(self):
        result = gate.supervise([str(self.directory/"missing-oracle")],
            deadline=time.monotonic()+2, output_path=self.directory/"output.log",
            artifact_dir=self.directory, limits=self.limits)
        self.assertEqual(result["status"], "error")
        self.assertIsNone(result["returncode"])

    def test_termination_signal_shuts_down_worker_before_reporting(self):
        pidfile, report = self.directory/"signaled.pid", self.directory/"signal-report.json"
        child = "import os,time; open("+repr(str(pidfile))+",'w').write(str(os.getpid())); time.sleep(60)"
        driver = (
            "import sys,signal,time; from pathlib import Path; "
            f"sys.path.insert(0,{str(gate.ROOT/'scripts')!r}); import semantic_gate as g; "
            "signal.signal(signal.SIGTERM,g.handle_shutdown_signal); "
            f"r=g.supervise([sys.executable,'-c',{child!r}],deadline=time.monotonic()+10,"
            f"output_path=Path({str(self.directory/'signal.log')!r}),artifact_dir=Path({str(self.directory)!r})); "
            f"g.write_json(Path({str(report)!r}),r)"
        )
        process = subprocess.Popen([sys.executable,"-c",driver], stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE, start_new_session=True)
        try:
            deadline = time.monotonic()+3
            while not pidfile.exists() and process.poll() is None and time.monotonic()<deadline:
                time.sleep(.01)
            self.assertTrue(pidfile.exists())
            process.send_signal(signal.SIGTERM)
            _, stderr = process.communicate(timeout=3)
            self.assertEqual(process.returncode, 0, stderr.decode())
            result = json.loads(report.read_text())
            self.assertEqual(result["status"], "error")
            self.assertIn("SIGTERM", result["error"])
            self.assert_dead(int(pidfile.read_text()))
        finally:
            if process.poll() is None:
                os.killpg(process.pid,signal.SIGKILL)
                process.communicate(timeout=3)
            if pidfile.exists():
                try:
                    os.kill(int(pidfile.read_text()),signal.SIGKILL)
                except ProcessLookupError:
                    pass


class ReportTests(unittest.TestCase):
    def setUp(self):
        self.plan = {"schema":1, "planned": [{"id":"a", "input_sha256":"digest", "facet":"valid", "case":{"families":["lambda"]}}],
            "pin_sha256":"pin", "profile_sha256":"profile", "manifest_sha256":"manifest"}
        self.report = {"schema":1, "status":"pass", "completed":[{"id":"a", "input_sha256":"digest", "oracle_sha256":"0"*64}],
            "counts":{**dict.fromkeys(("planned", "validated", "compared", "matched", "valid_planned"),1),
                "invalid_planned":0, "recovery_planned":0, "intended_invalid":0, "recovery":0},
            "failures":[], **{key:self.plan[key] for key in
                ("pin_sha256", "profile_sha256", "manifest_sha256")},
            "coverage":{"lambda":{"planned":1,"validated":1,"compared":1,"matched":1}},
            "stdio":{"planned":2,"completed":2,"passed":2},
            "isolation":{"aba":True,"fresh":True,"repeated":True,"reversed_views":1,
                "reversed_history":[{"id":"a","oracle_sha256":"0"*64}]},
            "metrics":{"histories":{"workers":2,"candidate_pid":103,"fresh_pid":104,
                "forward":{"pid":101,"planned":1,"completed":1},
                "reverse":{"pid":102,"planned":1,"completed":1,"complete":True,"cases":[{"id":"a"}]}}}}
        self.report["planned"] = copy.deepcopy(self.report["completed"])

    def test_complete_report_passes(self):
        gate.validate_report(self.plan, self.report)

    def test_reverse_coverage_and_session_independence_fail_closed(self):
        for mutation in ("omitted","duplicate","wrong-hash","incomplete","shared-session","not-repeated"):
            report=copy.deepcopy(self.report)
            if mutation=="omitted":report["isolation"]["reversed_history"]=[]
            elif mutation=="duplicate":report["isolation"]["reversed_history"]*=2
            elif mutation=="wrong-hash":report["isolation"]["reversed_history"][0]["oracle_sha256"]="1"*64
            elif mutation=="incomplete":report["metrics"]["histories"]["reverse"]["complete"]=False
            elif mutation=="shared-session":report["metrics"]["histories"]["reverse"]["pid"]=101
            else:report["isolation"]["repeated"]=False
            with self.subTest(mutation=mutation),self.assertRaises(ValueError):
                gate.validate_report(self.plan,report)

    def test_reverse_order_cannot_be_replaced_with_forward_order(self):
        plan=copy.deepcopy(self.plan);report=copy.deepcopy(self.report)
        plan["planned"].append({**copy.deepcopy(plan["planned"][0]),"id":"b","input_sha256":"second"})
        second={"id":"b","input_sha256":"second","oracle_sha256":"1"*64}
        report["planned"].append(second);report["completed"].append(second)
        for field in ("planned","validated","compared","matched","valid_planned"):report["counts"][field]=2
        report["coverage"]["lambda"]=dict.fromkeys(("planned","validated","compared","matched"),2)
        history=[{"id":"b","oracle_sha256":"1"*64},{"id":"a","oracle_sha256":"0"*64}]
        report["isolation"].update(reversed_views=2,reversed_history=history)
        report["metrics"]["histories"]["forward"].update(planned=2,completed=2)
        report["metrics"]["histories"]["reverse"].update(planned=2,completed=2,cases=[{"id":"b"},{"id":"a"}])
        gate.validate_report(plan,report)
        report["isolation"]["reversed_history"].reverse()
        with self.assertRaisesRegex(ValueError,"reordered"):
            gate.validate_report(plan,report)

    def test_unknown_schema_and_missing_family_fail(self):
        with self.assertRaises(ValueError):
            gate.validate_report(self.plan, {**self.report,"schema":2})
        with self.assertRaises(ValueError):
            gate.validate_report(self.plan, {**self.report,"coverage":{}})

    def test_missing_partial_duplicate_and_stale_cases_fail(self):
        for completed in ([], self.report["completed"]*2,
                [{"id":"a", "input_sha256":"stale"}], [{"id":"different", "input_sha256":"digest"}]):
            with self.subTest(completed=completed), self.assertRaises(ValueError):
                gate.validate_report(self.plan, {**self.report,"completed":completed})

    def test_zero_comparisons_fail(self):
        self.report["counts"]["compared"] = 0
        with self.assertRaises(ValueError):
            gate.validate_report(self.plan, self.report)
        with self.assertRaises(ValueError):
            gate.validate_report({**self.plan,"planned":[]}, self.report)

    def test_provenance_failure_and_protocol_omission_fail(self):
        variants = []
        for field, value in (("status","error"), ("failures",["witness"]),
                ("isolation",{"aba":False,"fresh":True}),
                ("stdio",{"planned":0,"completed":0,"passed":0}),
                ("pin_sha256","other")):
            variants.append({**self.report,field:value})
        for report in variants:
            with self.subTest(report=report), self.assertRaises(ValueError):
                gate.validate_report(self.plan, report)

    def test_missing_and_partial_json_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"report.json"
            with self.assertRaises(ValueError):
                gate.bounded_json(path, 100)
            path.write_text('{"status":')
            with self.assertRaises(ValueError):
                gate.bounded_json(path, 100)

    def test_invalid_and_recovery_lanes_must_be_complete(self):
        for lane in ("invalid","recovery"):
            with self.subTest(lane=lane):
                extra = {"id":lane, "input_sha256":"other", "facet":"compiler-error" if lane=="invalid" else "recovery", "case":{"families":[lane]}}
                plan = {**self.plan,"planned":[*self.plan["planned"],extra]}
                report = copy.deepcopy(self.report)
                report["planned"].append({"id":lane,"input_sha256":"other"})
                report["completed"].append({"id":lane,"input_sha256":"other","oracle_sha256":"1"*64})
                report["counts"].update(planned=2, **{f"{lane}_planned":1})
                report["metrics"]["histories"]["forward"].update(planned=2,completed=2)
                with self.assertRaises(ValueError):
                    gate.validate_report(plan, report)
                report["counts"]["intended_invalid" if lane=="invalid" else "recovery"] = 1
                report["coverage"][lane] = {"planned":1,"validated":0,"compared":0,"matched":0}
                gate.validate_report(plan, report)


class RuntimeDownloadTests(unittest.TestCase):
    def runtime_fixture(self):
        archive = io.BytesIO()
        with zipfile.ZipFile(archive, "w") as package:
            package.writestr("bun-test/bun", b"pinned-bun")
        data = archive.getvalue()
        artifact = {"name":"bun-test.zip", "asset":12345, "size":len(data),
            "sha256":installer.digest(data)}
        return data, {"version":"test", "artifacts":{"linux-x86_64":artifact}}

    def test_runtime_download_authenticates_only_fixed_asset_request(self):
        data, pin = self.runtime_fixture()
        def download(request, timeout):
            self.assertEqual(request.full_url,
                "https://api.github.com/repos/oven-sh/bun/releases/assets/12345")
            self.assertEqual(request.get_header("Accept"), "application/octet-stream")
            self.assertEqual(request.get_header("Authorization"), "Bearer test-read-only")
            redirected = urllib.request.HTTPRedirectHandler().redirect_request(
                request, None, 302, "redirect", {}, "https://release-assets.githubusercontent.com/runtime.zip")
            self.assertFalse(redirected.has_header("Authorization"))
            return io.BytesIO(data)
        with tempfile.TemporaryDirectory() as directory, \
                patch.object(installer, "CACHE", Path(directory)), \
                patch.object(installer.platform, "system", return_value="Linux"), \
                patch.object(installer.platform, "machine", return_value="x86_64"), \
                patch.dict(os.environ, {"GH_TOKEN":"test-read-only"}), \
                patch.object(bootstrap_bend.urllib.request, "urlopen", side_effect=download) as fetch:
            runtime = installer.runtime_path(download=True, runtime_pin=pin)
            self.assertEqual(Path(runtime).read_bytes(), b"pinned-bun")
            self.assertEqual(installer.runtime_path(runtime_pin=pin), runtime)
            self.assertEqual(fetch.call_count, 1)

    def test_runtime_download_rejects_wrong_size_and_checksum_without_retry(self):
        data, pin = self.runtime_fixture()
        for content in (data[:-1], data+b"x", b"x"*len(data)):
            with self.subTest(size=len(content)), tempfile.TemporaryDirectory() as directory, \
                    patch.object(installer, "CACHE", Path(directory)), \
                    patch.object(installer.platform, "system", return_value="Linux"), \
                    patch.object(installer.platform, "machine", return_value="x86_64"), \
                    patch.object(bootstrap_bend.urllib.request, "urlopen",
                        return_value=io.BytesIO(content)) as fetch:
                with self.assertRaisesRegex(ValueError, "archive size/checksum mismatch"):
                    installer.runtime_path(download=True, runtime_pin=pin)
                self.assertEqual(fetch.call_count, 1)
                self.assertFalse(list(Path(directory).rglob("runtime.zip")))


class WorkspaceBuildTests(unittest.TestCase):
    def test_workspace_dependency_sources_and_manifests_affect_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root/"Cargo.toml").write_text(
                '[workspace]\nmembers = [".", "tools/perf", "vendor/tower-lsp"]\n')
            for name in ("Cargo.lock", "rust-toolchain.toml"):
                (root/name).write_text(name)
            for member in ("tools/perf", "vendor/tower-lsp"):
                (root/member/"src").mkdir(parents=True)
                (root/member/"Cargo.toml").write_text(member)
                (root/member/"src/lib.rs").write_text("// original\n")
            with patch.object(gate, "ROOT", root):
                original = gate.source_identity()
                for relative in ("vendor/tower-lsp/src/lib.rs", "vendor/tower-lsp/Cargo.toml",
                        "tools/perf/src/lib.rs", "tools/perf/Cargo.toml"):
                    path = root/relative
                    contents = path.read_text()
                    path.write_text(contents+"# changed\n")
                    self.assertNotEqual(gate.source_identity(), original, relative)
                    path.write_text(contents)


class CalibrationTests(unittest.TestCase):
    def complete_evidence(self, directory):
        paths = []
        for job_index in range(5):
            root = Path(directory)/f"job-{job_index}"
            common = {"schema":1, "status":"pass", "provenance":{"pin":"fixed-tool"},
                "candidate_sha256":"fixed-candidate", "supervisor_profile_sha256":"fixed-profile",
                "environment":{"source_sha256":"fixed-source","ci":{"GITHUB_SHA":"same-revision",
                    "GITHUB_RUN_ID":"one-run","GITHUB_RUN_ATTEMPT":"1","GITHUB_JOB":"calibration",
                    "SEMANTIC_CALIBRATION_JOB":f"job-{job_index}"}}}
            gate.write_json(root/"runtime-install-report.json",{**common,"elapsed_seconds":2})
            prepared = {**common,"elapsed_seconds":3,"total_tool_preparation_seconds":5,
                "stages":{"oracle":{"status":"pass","elapsed_seconds":1},
                    "candidate":{"status":"pass","elapsed_seconds":2}}}
            gate.write_json(root/"setup-report.json",prepared)
            gate.write_json(root/"warm-setup/setup-report.json",prepared)
            scaling = root/"scaling/scaling-fixture"
            gate.write_json(scaling/"startup-worker-report.json",{
                "schema":1,"status":"pass","mode":"startup-only","generated_projects":0,
                "compared_views":1,"provenance":common["provenance"],
                "metrics":{"prepare_seconds":.1,"startup_first_reply_seconds":.2,"elapsed_seconds":.3}})
            summary = {**common,"startup":{"status":"pass","elapsed_seconds":.5,
                "peak_process_tree_rss_bytes":1024},"scaling":[]}
            for projects in (128,256,512):
                gate.write_json(scaling/f"projects-{projects}/attempt-fixture/scaling-report.json",
                    {**common,"mode":"calibration-scaling","elapsed_seconds":projects/128,
                        "counts":{"generated":projects}})
                summary["scaling"].append({"projects":projects,"status":"pass","elapsed_seconds":projects/128})
            gate.write_json(scaling/"scaling-summary.json",summary)
            for index in range(4):
                elapsed = job_index*4+index+1
                path = root/f"run-{index:02d}/attempt-fixture/gate-report.json"
                gate.write_json(path,{**common,"mode":"pr","elapsed_seconds":elapsed,
                    "measurement_id":f"{job_index*4+index+1:032x}",
                    "calibration_run_ordinal":index+1,
                    "peak_process_tree_rss_bytes":1024,"workload_digest":"fixed",
                    "stages":{"semantic":{"elapsed_seconds":elapsed}}})
                paths.append(path)
        return paths

    def test_local_or_insufficient_ci_evidence_cannot_calibrate(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"run.json"
            path.write_text(json.dumps({"environment":{"ci":{"GITHUB_SHA":None,
                "GITHUB_RUN_ID":None,"GITHUB_RUN_ATTEMPT":None,"GITHUB_JOB":None}}}))
            with self.assertRaises(ValueError):
                gate.calibration_report([path])
            with self.assertRaises(ValueError):
                gate.calibration_report([path]*20)

    def test_nearest_rank_calibration_requires_distinct_reports_and_jobs(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = self.complete_evidence(directory)
            report = gate.calibration_report(paths)
            self.assertEqual(report["runs"], 20)
            self.assertEqual(report["jobs"], 5)
            self.assertEqual(report["p50_seconds"], 10)
            self.assertEqual(report["p95_seconds"], 19)
            self.assertEqual(report["max_seconds"], 20)
            self.assertTrue(report["targets_met"])
            self.assertTrue(report["evidence_complete"])
            for phase in ("runtime_install","cold_setup","warm_setup","startup_supervised",
                    "scaling_128","scaling_256","scaling_512","gate_semantic"):
                self.assertIn(phase,report["stage_distributions"])

    def test_twenty_runs_cannot_substitute_for_four_distinct_runs_per_job(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = self.complete_evidence(directory)
            skewed = paths[:4]+[paths[index] for index in (4,8,12,16)]
            template = json.loads(paths[0].read_text())
            for ordinal in range(5,17):
                path = Path(directory)/"job-0"/f"run-{ordinal:02d}/attempt-fixture/gate-report.json"
                gate.write_json(path, {**template, "measurement_id":f"{100+ordinal:032x}",
                    "calibration_run_ordinal":ordinal})
                skewed.append(path)
            self.assertEqual(len(skewed), 20)
            with self.assertRaisesRegex(ValueError, "four distinct sequential runs per CI job"):
                gate.calibration_report(skewed)

    def test_copying_passing_measurement_to_distinct_paths_cannot_calibrate(self):
        with tempfile.TemporaryDirectory() as directory:
            paths = self.complete_evidence(directory)
            paths[1].write_bytes(paths[0].read_bytes())
            with self.assertRaisesRegex(ValueError, "duplicate or invalid calibration measurement identity"):
                gate.calibration_report(paths)

    def test_missing_or_invalid_measurement_identity_and_ordinal_fail_closed(self):
        for field, value in (("measurement_id",None), ("measurement_id","not-a-measurement"),
                ("calibration_run_ordinal",None), ("calibration_run_ordinal",True),
                ("calibration_run_ordinal",0), ("calibration_run_ordinal",1),
                ("calibration_run_ordinal",6)):
            with self.subTest(field=field,value=value), tempfile.TemporaryDirectory() as directory:
                paths = self.complete_evidence(directory)
                report = json.loads(paths[1].read_text())
                if value is None:
                    report.pop(field)
                else:
                    report[field] = value
                gate.write_json(paths[1],report)
                with self.assertRaises(ValueError):
                    gate.calibration_report(paths)

    def test_twenty_passing_runs_cannot_hide_missing_or_failed_support(self):
        variants = (
            ("runtime-install-report.json",None),
            ("setup-report.json",{"status":"error"}),
            ("warm-setup/setup-report.json",None),
            ("scaling/scaling-fixture/scaling-summary.json",{"status":"error"}),
            ("scaling/scaling-fixture/startup-worker-report.json",None),
            ("scaling/scaling-fixture/projects-512/attempt-fixture/scaling-report.json",{"status":"error"}),
            ("scaling/scaling-fixture/projects-128/attempt-fixture/scaling-report.json",{"candidate_sha256":"other"}),
            ("setup-report.json",{"provenance":{"pin":"other"}}),
        )
        for relative, changed in variants:
            with self.subTest(relative=relative,changed=changed), tempfile.TemporaryDirectory() as directory:
                paths = self.complete_evidence(directory)
                path = Path(directory)/"job-0"/relative
                if changed is None:
                    path.unlink()
                else:
                    gate.write_json(path,{**json.loads(path.read_text()),**changed})
                with self.assertRaises(ValueError):
                    gate.calibration_report(paths)

    def test_supporting_stage_must_match_source_and_revision(self):
        for field in ("source_sha256","revision"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as directory:
                paths = self.complete_evidence(directory)
                path = Path(directory)/"job-0/warm-setup/setup-report.json"
                report = json.loads(path.read_text())
                if field=="source_sha256":
                    report["environment"]["source_sha256"]="other"
                else:
                    report["environment"]["ci"]["GITHUB_SHA"]="other"
                gate.write_json(path,report)
                with self.assertRaises(ValueError):
                    gate.calibration_report(paths)


if __name__ == "__main__":
    unittest.main()
