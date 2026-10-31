//! Persistent, scoped Darwin process snapshots for the semantic supervisor.

use crate::ToolResult;

pub(super) fn run(arguments: &[String]) -> ToolResult<()> {
    if !arguments.is_empty() {
        return Err("Usage: bend2-perf workflow observe-processes (no arguments)".into());
    }
    #[cfg(target_os = "macos")]
    {
        macos::run()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("Native process observation is unsupported on this platform; macOS is required".into())
    }
}

#[cfg(any(target_os = "macos", test))]
mod protocol {
    use crate::ToolResult;
    use serde::{Deserialize, Serialize};
    use std::io::{self, BufRead, Write};

    // Both limits include the terminating newline, just like the supervisor's limits.
    pub(super) const MAX_REQUEST_BYTES: usize = 1024 * 1024;
    pub(super) const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Request {
        pub(super) schema: u32,
        pub(super) root: u32,
        pub(super) tracked: Vec<u32>,
        pub(super) groups: Vec<u32>,
    }

    #[derive(Debug, Serialize)]
    pub(super) struct Process(
        pub(super) u32,
        pub(super) u32,
        pub(super) u32,
        pub(super) u64,
        pub(super) String,
        pub(super) &'static str,
    );

    #[derive(Debug, Serialize)]
    pub(super) struct Response {
        pub(super) schema: u32,
        pub(super) observer_pid: u32,
        pub(super) processes: Vec<Process>,
    }

    pub(super) fn read_frame(reader: &mut impl BufRead, frame: &mut Vec<u8>) -> ToolResult<bool> {
        frame.clear();
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return if frame.is_empty() {
                    Ok(false)
                } else {
                    Err("Process observer request ended before its newline".into())
                };
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(available.len(), |index| index + 1);
            if count > MAX_REQUEST_BYTES - frame.len() {
                return Err("Process observer request exceeds 1 MiB including its newline".into());
            }
            frame.extend_from_slice(&available[..count]);
            reader.consume(count);
            if newline.is_some() {
                return Ok(true);
            }
            if frame.len() == MAX_REQUEST_BYTES {
                return Err("Process observer request exceeds 1 MiB including its newline".into());
            }
        }
    }

    pub(super) fn parse_request(frame: &[u8]) -> ToolResult<Request> {
        let request: Request = serde_json::from_slice(frame)?;
        if request.schema != 1 {
            return Err("Unsupported process observer request schema".into());
        }
        for pid in std::iter::once(&request.root)
            .chain(&request.tracked)
            .chain(&request.groups)
        {
            if *pid == 0 || i32::try_from(*pid).is_err() {
                return Err(
                    format!("Process observer PID/group {pid} is outside 1..=i32::MAX").into(),
                );
            }
        }
        Ok(request)
    }

    struct BoundedOutput<'a>(&'a mut Vec<u8>);

    impl Write for BoundedOutput<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > MAX_RESPONSE_BYTES - 1 - self.0.len() {
                return Err(io::Error::other(
                    "Process observer response exceeds 16 MiB including its newline",
                ));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub(super) fn write_response(
        writer: &mut impl Write,
        response: &Response,
        output: &mut Vec<u8>,
    ) -> ToolResult<()> {
        output.clear();
        // Serialize fully within the cap before publishing any part of a response.
        serde_json::to_writer(BoundedOutput(output), response)?;
        output.push(b'\n');
        writer.write_all(output)?;
        writer.flush()?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufReader, Cursor};

        const REQUEST: &[u8] = b"{\"schema\":1,\"root\":17,\"tracked\":[18],\"groups\":[19]}\n";

        #[test]
        fn framing_keeps_partial_reads_and_consecutive_requests_separate() -> ToolResult<()> {
            let bytes = [REQUEST, REQUEST].concat();
            let mut reader = BufReader::with_capacity(3, Cursor::new(bytes));
            let mut frame = Vec::new();
            for _ in 0..2 {
                assert!(read_frame(&mut reader, &mut frame)?);
                assert_eq!(frame, REQUEST);
                let request = parse_request(&frame)?;
                assert_eq!(request.root, 17);
                assert_eq!(request.tracked, [18]);
                assert_eq!(request.groups, [19]);
            }
            assert!(!read_frame(&mut reader, &mut frame)?);
            Ok(())
        }

        #[test]
        fn request_limit_counts_newline_and_accepts_exact_boundary() -> ToolResult<()> {
            let mut bytes = REQUEST[..REQUEST.len() - 1].to_vec();
            bytes.resize(MAX_REQUEST_BYTES - 1, b' ');
            bytes.push(b'\n');
            let mut frame = Vec::new();
            assert!(read_frame(&mut Cursor::new(&bytes), &mut frame)?);
            assert_eq!(frame.len(), MAX_REQUEST_BYTES);
            assert_eq!(parse_request(&frame)?.root, 17);
            bytes.insert(bytes.len() - 1, b' ');
            assert!(read_frame(&mut Cursor::new(bytes), &mut frame).is_err());
            assert!(
                read_frame(&mut Cursor::new(vec![b' '; MAX_REQUEST_BYTES]), &mut frame).is_err()
            );
            Ok(())
        }

        #[test]
        fn eof_does_not_complete_a_request_and_bad_data_is_rejected() -> ToolResult<()> {
            let mut frame = Vec::new();
            assert!(
                read_frame(&mut Cursor::new(&REQUEST[..REQUEST.len() - 1]), &mut frame).is_err()
            );
            for invalid in [
                "\n",
                "{}\n",
                "{\"schema\":2,\"root\":1,\"tracked\":[],\"groups\":[]}\n",
                "{\"schema\":true,\"root\":1,\"tracked\":[],\"groups\":[]}\n",
                "{\"schema\":1,\"root\":true,\"tracked\":[],\"groups\":[]}\n",
                "{\"schema\":1,\"root\":1.0,\"tracked\":[],\"groups\":[]}\n",
                "{\"schema\":1,\"root\":0,\"tracked\":[],\"groups\":[]}\n",
                "{\"schema\":1,\"root\":2147483648,\"tracked\":[],\"groups\":[]}\n",
                "{\"schema\":1,\"root\":1,\"tracked\":[0],\"groups\":[]}\n",
                "{\"schema\":1,\"root\":1,\"tracked\":[],\"groups\":[false]}\n",
                "{\"schema\":1,\"root\":1,\"tracked\":[],\"groups\":[-1]}\n",
                "{\"schema\":1,\"root\":1,\"tracked\":[],\"groups\":[],\"extra\":1}\n",
                "{\"schema\":1,\"schema\":1,\"root\":1,\"tracked\":[],\"groups\":[]}\n",
                "{\"schema\":1,\"root\":1,\"tracked\":[],\"groups\":[]} {}\n",
            ] {
                assert!(
                    parse_request(invalid.as_bytes()).is_err(),
                    "accepted {invalid:?}"
                );
            }
            assert!(parse_request(b"\xff\n").is_err());
            assert_eq!(parse_request(REQUEST)?.schema, 1);
            Ok(())
        }

        #[test]
        fn response_limit_is_atomic_and_includes_the_terminator() -> ToolResult<()> {
            let mut response = Response {
                schema: 1,
                observer_pid: 17,
                processes: vec![Process(17, 18, 19, 4096, String::new(), "R")],
            };
            let mut destination = Vec::new();
            let mut output = Vec::new();
            write_response(&mut destination, &response, &mut output)?;
            let overhead = destination.len();
            response.processes[0].4 = "x".repeat(MAX_RESPONSE_BYTES - overhead);
            destination.clear();
            write_response(&mut destination, &response, &mut output)?;
            assert_eq!(destination.len(), MAX_RESPONSE_BYTES);
            assert_eq!(destination.last(), Some(&b'\n'));
            let decoded: serde_json::Value = serde_json::from_slice(&destination)?;
            assert_eq!(decoded["processes"][0][3], 4096);
            assert_eq!(decoded["observer_pid"], 17);
            response.processes[0].4.push('x');
            destination.clear();
            assert!(write_response(&mut destination, &response, &mut output).is_err());
            assert!(destination.is_empty());
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::protocol::{self, Process, Request, Response};
    use crate::ToolResult;
    use libproc::{
        bsd_info::BSDInfo,
        proc_pid::pidinfo,
        processes::{ProcFilter, pids_by_type},
        task_info::TaskInfo,
    };
    use nix::{errno::Errno, sys::signal::kill, unistd::Pid};
    use std::{
        collections::{BTreeSet, VecDeque},
        io,
    };

    // XNU bsd/sys/proc_info.h: PROC_FLAG_INEXIT maps to kernel P_LEXIT.
    const PROC_FLAG_INEXIT: u32 = 4;

    pub(super) fn run() -> ToolResult<()> {
        serve(&mut io::stdin().lock(), &mut io::stdout().lock())
    }

    fn serve(reader: &mut impl io::BufRead, writer: &mut impl io::Write) -> ToolResult<()> {
        let mut frame = Vec::new();
        let mut output = Vec::new();
        while protocol::read_frame(reader, &mut frame)? {
            let request = protocol::parse_request(&frame)?;
            let response = snapshot(&request)?;
            protocol::write_response(writer, &response, &mut output)?;
        }
        Ok(())
    }

    fn list(filter: ProcFilter) -> ToolResult<Vec<u32>> {
        // libproc 0.14.11 treats a zero-length result plus stale errno as an error.
        // Empty child/group sets are normal; clear thread-local errno before the call.
        Errno::clear();
        let pids = pids_by_type(filter)
            .map_err(|error| format!("Scoped process discovery failed: {error}"))?;
        // The pinned wrapper performs one sizing call and one fill call. A full
        // buffer could have truncated a racing fork burst: fail, never omit live PIDs.
        if !pids.is_empty() && pids.len() == pids.capacity() {
            return Err(
                "Scoped process discovery filled its buffer; snapshot may be incomplete".into(),
            );
        }
        Ok(pids)
    }

    fn snapshot(request: &Request) -> ToolResult<Response> {
        let observer = std::process::id();
        let mut candidates = BTreeSet::from([request.root, observer]);
        candidates.extend(&request.tracked);
        // A group leader can already be an unreaped zombie before its first
        // observation, even when the kernel group-member filter no longer lists it.
        candidates.extend(&request.groups);
        for group in &request.groups {
            candidates.extend(list(ProcFilter::ByProgramGroup { pgrpid: *group })?);
        }
        let mut pending: VecDeque<u32> = candidates.iter().copied().collect();
        let mut processes = Vec::new();
        while let Some(pid) = pending.pop_front() {
            if pid == 0 || i32::try_from(pid).is_err() {
                return Err(format!("Scoped discovery returned invalid PID {pid}").into());
            }
            // Discover even a vanished tracked parent's children. A detached descendant's
            // own tracked PID/group remains a seed after reparenting to launchd.
            for child in list(ProcFilter::ByParentProcess { ppid: pid })? {
                if candidates.insert(child) {
                    pending.push_back(child);
                }
            }
            if let Some(process) = process(pid)? {
                processes.push(process);
            }
        }
        if !processes.iter().any(|process| process.0 == observer) {
            return Err("Native observer snapshot omitted its own process".into());
        }
        processes.sort_unstable_by_key(|process| process.0);
        Ok(Response {
            schema: 1,
            observer_pid: observer,
            processes,
        })
    }

    fn native_failure<T>(pid: u32, operation: &str, error: &str) -> ToolResult<Option<T>> {
        // The safe pinned API returns String rather than typed errno. Only an
        // independent exact-PID ESRCH proves disappearance; EPERM is not absence.
        match kill(Pid::from_raw(i32::try_from(pid)?), None) {
            Err(Errno::ESRCH) => Ok(None),
            Ok(()) => Err(format!("{operation} failed for existing PID {pid}: {error}").into()),
            Err(probe) => Err(format!(
                "{operation} failed for PID {pid}: {error}; existence probe failed: {probe}"
            )
            .into()),
        }
    }

    fn bsd(pid: u32) -> ToolResult<Option<BSDInfo>> {
        // arg=1 includes unreaped zombies; arg=0 misreports them as ESRCH.
        match pidinfo::<BSDInfo>(i32::try_from(pid)?, 1) {
            Ok(info) => Ok(Some(info)),
            Err(error) => native_failure(pid, "proc_pidinfo(BSDInfo)", &error),
        }
    }

    fn identity(info: &BSDInfo) -> (u64, u64) {
        (info.pbi_start_tvsec, info.pbi_start_tvusec)
    }

    fn native_errno(error: &str) -> Option<Errno> {
        // Exact, pinned libproc::helpers::get_errno_with_message format. Do not
        // infer errno from localized strerror text or a potentially changed TLS errno.
        let (_, suffix) = error.split_once(", errno = ")?;
        let (number, _) = suffix.split_once(", message = ")?;
        number.parse().ok().map(Errno::from_raw)
    }

    fn taskless_exit_state(status: u32, flags: u32, error: Option<Errno>) -> Option<&'static str> {
        if error != Some(Errno::ESRCH) {
            return None;
        }
        if status == 5 {
            Some("Z")
        } else if flags & PROC_FLAG_INEXIT != 0 {
            Some("X")
        } else {
            None
        }
    }

    fn process(pid: u32) -> ToolResult<Option<Process>> {
        let Some(before) = bsd(pid)? else {
            return Ok(None);
        };
        let resident = pidinfo::<TaskInfo>(i32::try_from(pid)?, 0);
        let Some(after) = bsd(pid)? else {
            return Ok(None);
        };
        if before.pbi_pid != pid || after.pbi_pid != pid || identity(&before) != identity(&after) {
            return Err(
                format!("PID {pid} changed kernel start identity during observation").into(),
            );
        }
        if after.pbi_ppid == 0 || after.pbi_pgid == 0 || after.pbi_start_tvusec >= 1_000_000 {
            return Err(format!("PID {pid} returned invalid parent/group/start metadata").into());
        }
        let (resident, state) = match resident {
            Ok(task) => (
                task.pti_resident_size,
                if after.pbi_status == 5 { "Z" } else { "R" },
            ),
            Err(error) => {
                match taskless_exit_state(after.pbi_status, after.pbi_flags, native_errno(&error)) {
                    Some(state) => {
                        // proc_find rejects dead references before BSD state necessarily
                        // becomes SZOMB. Exact ESRCH plus same-birth INEXIT or SZOMB
                        // proves no readable current task, matching ps's zero task RSS.
                        // Never use pidrusage's cached exit resident size or hide a
                        // permission/live read failure. Successful reads above survive.
                        (0, state)
                    }
                    None => return native_failure(pid, "proc_pidinfo(TaskInfo)", &error),
                }
            }
        };
        let (seconds, micros) = identity(&after);
        Ok(Some(Process(
            pid,
            after.pbi_ppid,
            after.pbi_pgid,
            resident / 1024 * 1024,
            format!("{seconds}:{micros:06}"),
            state,
        )))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use nix::sys::{
            signal::{Signal, killpg},
            wait::{WaitPidFlag, WaitStatus, waitpid},
        };
        use serde_json::{Value, json};
        use std::{
            io::{BufRead, BufReader, Read, Write},
            os::unix::process::CommandExt,
            process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
            thread,
            time::{Duration, Instant},
        };

        #[test]
        fn unavailable_task_requires_kernel_exit_proof_not_zero_rss_or_permissions() {
            assert_eq!(taskless_exit_state(2, 0, Some(Errno::ESRCH)), None);
            assert_eq!(taskless_exit_state(2, 0x10, Some(Errno::ESRCH)), None);
            assert_eq!(
                taskless_exit_state(2, PROC_FLAG_INEXIT, Some(Errno::ESRCH)),
                Some("X")
            );
            assert_eq!(taskless_exit_state(5, 0, Some(Errno::ESRCH)), Some("Z"));
            assert_eq!(
                taskless_exit_state(5, PROC_FLAG_INEXIT, Some(Errno::EPERM)),
                None
            );
            assert_eq!(taskless_exit_state(2, PROC_FLAG_INEXIT, None), None);
        }

        const MEMORY_FIXTURE: &str = r"
import mmap, sys
memory = None
print('ready', flush=True)
for line in sys.stdin:
    command = line.strip()
    if command == 'allocate':
        memory = mmap.mmap(-1, 64 * 1024 * 1024)
        for offset in range(0, len(memory), 4096):
            memory[offset] = 1
    elif command == 'release':
        memory.close()
        memory = None
    elif command == 'quit':
        break
    else:
        raise RuntimeError(command)
    print('ready', flush=True)
";

        const TREE_FIXTURE: &str = r"
import json, os, signal, subprocess, sys
worker = '''
import json, os, signal, subprocess, sys
child = subprocess.Popen(['/bin/sleep', '60'])
def stop(signum, frame):
    child.terminate()
    child.wait()
    sys.exit(0)
signal.signal(signal.SIGTERM, stop)
print(json.dumps([os.getpid(), child.pid, os.getpgrp()]), flush=True)
while True:
    signal.pause()
'''
ordinary = subprocess.Popen(['/bin/sleep', '60'])
escaped = subprocess.Popen([sys.executable, '-u', '-c', worker],
                           start_new_session=True, stdout=subprocess.PIPE, text=True)
try:
    descendants = json.loads(escaped.stdout.readline())
    print(json.dumps([ordinary.pid] + descendants), flush=True)
    for line in sys.stdin:
        if line.strip() == 'orphan':
            os._exit(0)
        if line.strip() == 'quit':
            break
        raise RuntimeError(line)
finally:
    ordinary.terminate()
    ordinary.wait()
    escaped.terminate()
    escaped.wait()
";

        struct OwnedProcess {
            child: Child,
            input: Option<ChildStdin>,
            output: BufReader<ChildStdout>,
            groups: Vec<u32>,
            reaped: bool,
        }

        impl OwnedProcess {
            fn spawn(command: &mut Command, own_group: bool) -> ToolResult<Self> {
                if own_group {
                    command.process_group(0);
                }
                let mut child = command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()?;
                let input = child.stdin.take().ok_or("Fixture stdin was not piped")?;
                let output = child.stdout.take().ok_or("Fixture stdout was not piped")?;
                let groups = if own_group {
                    vec![child.id()]
                } else {
                    Vec::new()
                };
                Ok(Self {
                    child,
                    input: Some(input),
                    output: BufReader::new(output),
                    groups,
                    reaped: false,
                })
            }

            fn python(script: &str) -> ToolResult<Self> {
                Self::spawn(Command::new("python3").args(["-u", "-c", script]), true)
            }

            fn observer() -> ToolResult<Self> {
                // Cargo's unit-test executable lives in target/<profile>/deps.
                // Build bend2-perf first: these tests exercise its real CLI/IPC,
                // not a substitute observer running inside the fixture's parent.
                let executable = std::env::current_exe()?;
                let binary = executable
                    .parent()
                    .and_then(std::path::Path::parent)
                    .ok_or("Cannot locate the built bend2-perf beside the unit-test executable")?
                    .join("bend2-perf");
                Self::spawn(
                    Command::new(&binary).args(["workflow", "observe-processes"]),
                    false,
                )
                .map_err(|error| {
                    format!(
                    "Cannot start native observer {}; build bend2-perf before these tests: {error}",
                    binary.display()
                ).into()
                })
            }

            fn send(&mut self, text: &str) -> ToolResult<()> {
                let input = self.input.as_mut().ok_or("Fixture stdin is closed")?;
                writeln!(input, "{text}")?;
                input.flush()?;
                Ok(())
            }

            fn line(&mut self) -> ToolResult<String> {
                let mut text = String::new();
                if self.output.read_line(&mut text)? == 0 {
                    return Err("Fixture closed stdout before its response".into());
                }
                Ok(text)
            }

            fn observe(&mut self, root: u32, tracked: &[u32], groups: &[u32]) -> ToolResult<Value> {
                self.send(
                    &json!({"schema":1, "root":root, "tracked":tracked, "groups":groups})
                        .to_string(),
                )?;
                let response: Value = serde_json::from_str(&self.line()?)?;
                assert_eq!(response["schema"], 1);
                assert_eq!(response["observer_pid"], self.child.id());
                let observer = row(&response, self.child.id())?;
                assert!(observer[3].as_u64().is_some_and(|rss| rss > 0));
                assert_eq!(observer[5], "R");
                assert_eq!(observer[2], u32::try_from(nix::unistd::getpgrp().as_raw())?);
                assert!(
                    list(ProcFilter::ByParentProcess {
                        ppid: self.child.id()
                    })?
                    .is_empty()
                );
                Ok(response)
            }

            fn reap(&mut self) -> ToolResult<ExitStatus> {
                self.input.take();
                let started = Instant::now();
                loop {
                    if let Some(status) = self.child.try_wait()? {
                        self.reaped = true;
                        return Ok(status);
                    }
                    if started.elapsed() >= Duration::from_secs(5) {
                        return Err("Fixture failed to exit within five seconds".into());
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }

            fn finish(&mut self) -> ToolResult<()> {
                assert!(self.reap()?.success());
                self.groups.clear();
                Ok(())
            }

            fn group_is_live(group: u32) -> ToolResult<bool> {
                for pid in list(ProcFilter::ByProgramGroup { pgrpid: group })? {
                    if bsd(pid)?.is_some_and(|info| info.pbi_pgid == group && info.pbi_status != 5)
                    {
                        return Ok(true);
                    }
                }
                Ok(false)
            }

            fn terminate_groups(&mut self) -> ToolResult<()> {
                for group in &self.groups {
                    if !Self::group_is_live(*group)? {
                        continue;
                    }
                    match killpg(Pid::from_raw(i32::try_from(*group)?), Signal::SIGTERM) {
                        Ok(()) | Err(Errno::ESRCH) => {}
                        Err(Errno::EPERM) if !Self::group_is_live(*group)? => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                let started = Instant::now();
                loop {
                    let mut live_group = false;
                    for group in &self.groups {
                        if Self::group_is_live(*group)? {
                            live_group = true;
                        }
                    }
                    if !live_group {
                        self.groups.clear();
                        return Ok(());
                    }
                    if started.elapsed() >= Duration::from_secs(5) {
                        return Err("Detached fixture groups survived termination".into());
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            }
        }

        impl Drop for OwnedProcess {
            fn drop(&mut self) {
                for group in &self.groups {
                    if let Ok(group) = i32::try_from(*group) {
                        let _ = killpg(Pid::from_raw(group), Signal::SIGKILL);
                    }
                }
                if !self.reaped {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                }
            }
        }

        fn row(response: &Value, pid: u32) -> ToolResult<&[Value]> {
            response["processes"]
                .as_array()
                .ok_or("Missing process rows")?
                .iter()
                .find(|row| row[0] == pid)
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .ok_or_else(|| format!("Missing process row for PID {pid}: {response}").into())
        }

        fn wait_for_zombie(pid: u32) -> ToolResult<()> {
            let started = Instant::now();
            loop {
                if bsd(pid)?.is_some_and(|info| info.pbi_status == 5) {
                    return Ok(());
                }
                if started.elapsed() >= Duration::from_secs(5) {
                    return Err("Fixture failed to become an unreaped zombie".into());
                }
                thread::sleep(Duration::from_millis(5));
            }
        }

        fn ps_resident_and_state(pid: u32) -> ToolResult<(u64, String)> {
            let output = Command::new("/bin/ps")
                .args(["-p", &pid.to_string(), "-o", "rss=,stat="])
                .output()?;
            if !output.status.success() {
                return Err(
                    format!("ps failed: {}", String::from_utf8_lossy(&output.stderr)).into(),
                );
            }
            let text = String::from_utf8(output.stdout)?;
            let mut fields = text.split_whitespace();
            let resident = fields
                .next()
                .ok_or("ps omitted resident memory")?
                .parse::<u64>()?
                * 1024;
            let state = fields.next().ok_or("ps omitted process state")?.to_owned();
            assert!(fields.next().is_none());
            Ok((resident, state))
        }

        #[test]
        fn persistent_observer_measures_current_rss_and_stable_kernel_identity() -> ToolResult<()> {
            let mut target = OwnedProcess::python(MEMORY_FIXTURE)?;
            assert_eq!(target.line()?.trim(), "ready");
            let pid = target.child.id();
            let mut observer = OwnedProcess::observer()?;
            let observer_pid = observer.child.id();
            let baseline = observer.observe(pid, &[], &[])?;
            let baseline = row(&baseline, pid)?;
            let baseline_rss = baseline[3].as_u64().ok_or("Missing baseline RSS")?;
            let birth = baseline[4].as_str().ok_or("Missing birth")?.to_owned();
            let info = bsd(pid)?.ok_or("Live fixture disappeared")?;
            assert_eq!(
                birth,
                format!("{}:{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec)
            );
            assert_eq!(baseline[1], std::process::id());
            target.send("allocate")?;
            assert_eq!(target.line()?.trim(), "ready");
            let allocated = observer.observe(pid, &[pid], &[pid])?;
            let allocated = row(&allocated, pid)?;
            let allocated_rss = allocated[3].as_u64().ok_or("Missing allocated RSS")?;
            assert!(allocated_rss >= baseline_rss + 32 * 1024 * 1024);
            assert_eq!(allocated[4], birth);
            target.send("release")?;
            assert_eq!(target.line()?.trim(), "ready");
            let released = observer.observe(pid, &[pid], &[pid])?;
            let released = row(&released, pid)?;
            let released_rss = released[3].as_u64().ok_or("Missing released RSS")?;
            assert!(released_rss + 32 * 1024 * 1024 <= allocated_rss);
            assert_eq!(released[4], birth);
            assert_eq!(observer.child.id(), observer_pid);
            target.send("quit")?;
            target.finish()?;
            observer.finish()?;
            Ok(())
        }

        #[test]
        fn root_tracked_and_group_closure_preserve_escaped_reparented_descendants() -> ToolResult<()>
        {
            let mut tree = OwnedProcess::python(TREE_FIXTURE)?;
            let ids: [u32; 4] = serde_json::from_str(&tree.line()?)?;
            let [ordinary, escaped, grandchild, escaped_group] = ids;
            tree.groups.push(escaped_group);
            let root = tree.child.id();
            let mut unrelated = OwnedProcess::python(MEMORY_FIXTURE)?;
            assert_eq!(unrelated.line()?.trim(), "ready");
            let mut observer = OwnedProcess::observer()?;
            let initial = observer.observe(root, &[], &[])?;
            assert_eq!(row(&initial, ordinary)?[1], root);
            assert_eq!(row(&initial, escaped)?[1], root);
            assert_eq!(row(&initial, grandchild)?[1], escaped);
            assert_eq!(row(&initial, ordinary)?[2], root);
            assert_eq!(row(&initial, escaped)?[2], escaped_group);
            assert_eq!(escaped_group, escaped);
            assert!(row(&initial, unrelated.child.id()).is_err());
            let escaped_birth = row(&initial, escaped)?[4].clone();
            tree.send("orphan")?;
            assert!(tree.reap()?.success());
            let tracked = observer.observe(root, &[escaped], &[])?;
            assert!(row(&tracked, root).is_err());
            assert_eq!(row(&tracked, escaped)?[4], escaped_birth);
            assert_eq!(row(&tracked, grandchild)?[1], escaped);
            assert_ne!(row(&tracked, escaped)?[1], root);
            let grouped = observer.observe(root, &[], &[root, escaped_group])?;
            assert_eq!(row(&grouped, ordinary)?[2], root);
            assert_eq!(row(&grouped, escaped)?[2], escaped_group);
            assert_eq!(row(&grouped, grandchild)?[2], escaped_group);
            assert!(row(&grouped, unrelated.child.id()).is_err());
            tree.terminate_groups()?;
            unrelated.send("quit")?;
            unrelated.finish()?;
            observer.finish()?;
            Ok(())
        }

        #[test]
        fn real_live_zombie_and_reaped_rows_match_ps_without_cached_exit_rss() -> ToolResult<()> {
            let mut target = OwnedProcess::python(MEMORY_FIXTURE)?;
            assert_eq!(target.line()?.trim(), "ready");
            target.send("allocate")?;
            assert_eq!(target.line()?.trim(), "ready");
            let pid = target.child.id();
            let numeric = Pid::from_raw(i32::try_from(pid)?);
            kill(numeric, Signal::SIGSTOP)?;
            assert_eq!(
                waitpid(numeric, Some(WaitPidFlag::WUNTRACED))?,
                WaitStatus::Stopped(numeric, Signal::SIGSTOP)
            );
            let live = process(pid)?.ok_or("Stopped live fixture disappeared")?;
            let (resident, state) = ps_resident_and_state(pid)?;
            assert_eq!(live.3, resident);
            assert!(resident >= 32 * 1024 * 1024);
            assert!(state.starts_with('T'));
            assert_eq!(live.5, "R");
            kill(numeric, Signal::SIGKILL)?;
            wait_for_zombie(pid)?;
            let zombie = process(pid)?.ok_or("Unreaped zombie disappeared")?;
            let (resident, state) = ps_resident_and_state(pid)?;
            assert!(state.starts_with('Z'));
            assert_eq!(zombie.5, "Z");
            assert_eq!(zombie.3, resident);
            assert_eq!(zombie.4, live.4);
            // A first sample can encounter a zombie group leader without a tracked
            // identity; the explicit group-ID candidate must retain its real row.
            let mut observer = OwnedProcess::observer()?;
            let observer_root = observer.child.id();
            let response = observer.observe(observer_root, &[], &[pid])?;
            assert_eq!(row(&response, pid)?[3], resident);
            assert_eq!(row(&response, pid)?[5], "Z");
            let status = target.reap()?;
            assert!(!status.success());
            target.groups.clear();
            assert!(process(pid)?.is_none());
            let mut replacement = OwnedProcess::python(MEMORY_FIXTURE)?;
            assert_eq!(replacement.line()?.trim(), "ready");
            let replacement_row =
                process(replacement.child.id())?.ok_or("Replacement disappeared")?;
            assert_ne!(replacement_row.4, live.4);
            let response = observer.observe(observer_root, &[pid], &[pid])?;
            assert!(row(&response, pid).is_err());
            assert!(row(&response, replacement.child.id()).is_err());
            replacement.send("quit")?;
            replacement.finish()?;
            observer.finish()?;
            Ok(())
        }

        #[test]
        fn native_failures_never_turn_live_or_permission_denied_pids_into_absence() -> ToolResult<()>
        {
            assert!(native_failure::<()>(std::process::id(), "fixture", "read denied").is_err());
            assert!(native_failure::<()>(1, "fixture", "permission denied").is_err());
            assert_eq!(
                native_errno("return code = 0, errno = 3, message = 'No such process'"),
                Some(Errno::ESRCH)
            );
            assert_eq!(
                native_errno("return code = 0, errno = 1, message = 'Operation not permitted'"),
                Some(Errno::EPERM)
            );
            assert_eq!(native_errno("unrecognized native failure"), None);
            let mut target = OwnedProcess::python(MEMORY_FIXTURE)?;
            assert_eq!(target.line()?.trim(), "ready");
            let pid = target.child.id();
            target.send("quit")?;
            target.finish()?;
            assert!(native_failure::<()>(pid, "fixture", "metadata unavailable")?.is_none());
            Ok(())
        }
        #[test]
        fn actual_loop_rejects_invalid_or_unterminated_frames_without_a_response() -> ToolResult<()>
        {
            for frame in [
                "{\"schema\":1,\"root\":17,\"tracked\":[],\"groups\":[]}",
                "{\"schema\":1,\"root\":true,\"tracked\":[],\"groups\":[]}\n",
                "{\"schema\":1,\"root\":17,\"tracked\":[],\"groups\":[],\"other\":1}\n",
                "\n",
            ] {
                let mut observer = OwnedProcess::observer()?;
                observer
                    .input
                    .as_mut()
                    .ok_or("Missing observer stdin")?
                    .write_all(frame.as_bytes())?;
                assert!(!observer.reap()?.success(), "accepted {frame:?}");
                let mut output = Vec::new();
                observer.output.read_to_end(&mut output)?;
                assert!(output.is_empty(), "invalid request produced {output:?}");
            }
            let mut observer = OwnedProcess::observer()?;
            let root = observer.child.id();
            let valid = observer.observe(root, &[], &[])?;
            assert_eq!(row(&valid, root)?[0], root);
            observer.send("")?;
            assert!(!observer.reap()?.success());
            let mut output = Vec::new();
            observer.output.read_to_end(&mut output)?;
            assert!(
                output.is_empty(),
                "invalid second frame produced an extra response"
            );
            Ok(())
        }
    }
}

#[cfg(all(test, not(target_os = "macos")))]
mod unsupported_tests {
    use super::*;

    #[test]
    fn observer_is_explicitly_unsupported_off_macos() {
        let result = run(&[]);
        assert!(result.is_err());
        assert!(
            result
                .err()
                .is_some_and(|error| error.to_string().contains("unsupported"))
        );
    }
}
