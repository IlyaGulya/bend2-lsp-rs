use super::{
    Child, Command, Duration, File, Instant, OpenOptions, OsString, Path, PathBuf, ScenarioSession,
    Stdio, ToolResult, Value, doctor, fs, json, thread, validation, windows,
};
use std::io::{Read, Write};

#[cfg(all(test, target_os = "linux"))]
#[path = "linux_perf_tests.rs"]
mod linux_perf_tests;

#[derive(Clone, Copy)]
enum CommandMode {
    Active,
    Cleanup,
}

impl CommandMode {
    fn check(self) -> ToolResult<()> {
        match self {
            Self::Active => crate::scenario::check_cancelled(),
            Self::Cleanup => Ok(()),
        }
    }
}

pub(super) struct Session {
    backend: String,
    kind: String,
    output: PathBuf,
    child: Option<Child>,
    notifier: Option<Child>,
    control: Option<File>,
    wpr_active: bool,
    samply_etw_owned: bool,
    samply_elevated: bool,
    heap_image: Option<String>,
    heap_ifeo: Option<Value>,
    heap_ifeo_restored: Option<bool>,
    started: Instant,
    pub(super) pid: Option<u32>,
    pub(super) phases: Vec<Value>,
    pub(super) heap_summary: Value,
    commands: Vec<Value>,
    control_messages: Vec<Value>,
    version: Value,
    tools: Vec<Value>,
    trace: PathBuf,
    instance: String,
}

impl Session {
    pub(super) fn new(backend: &str, kind: &str, output: &Path) -> Self {
        let filename = match backend {
            "samply" => "samply.json",
            "dhat" => "dhat-heap.json",
            "perf" => "perf.data",
            "xctrace" => "native.trace",
            _ => "native.etl",
        };
        Self {
            backend: backend.to_owned(),
            kind: kind.to_owned(),
            output: output.to_owned(),
            child: None,
            notifier: None,
            control: None,
            wpr_active: false,
            samply_etw_owned: false,
            samply_elevated: false,
            heap_image: None,
            heap_ifeo: None,
            heap_ifeo_restored: None,
            started: Instant::now(),
            pid: None,
            phases: Vec::new(),
            heap_summary: Value::Null,
            commands: Vec::new(),
            control_messages: Vec::new(),
            version: Value::Null,
            tools: Vec::new(),
            trace: output.join(filename),
            instance: format!("bend2-perf-{}", std::process::id()),
        }
    }

    fn record(&mut self, program: &str, args: &[String]) {
        self.commands.push(json!({"program": program, "args": args, "elapsed_ns": self.started.elapsed().as_nanos()}));
    }

    fn command(&mut self, program: &str, args: &[String]) -> ToolResult<String> {
        self.command_env(program, args, &[])
    }

    fn command_env(
        &mut self,
        program: &str,
        args: &[String],
        environment: &[(&str, &str)],
    ) -> ToolResult<String> {
        self.command_env_mode(program, args, environment, CommandMode::Active)
    }

    fn cleanup_command(&mut self, program: &str, args: &[String]) -> ToolResult<String> {
        self.command_env_mode(program, args, &[], CommandMode::Cleanup)
    }

    fn command_env_mode(
        &mut self,
        program: &str,
        args: &[String],
        environment: &[(&str, &str)],
        mode: CommandMode,
    ) -> ToolResult<String> {
        self.command_env_mode_timeout(program, args, environment, mode, Duration::from_secs(300))
    }

    fn command_env_mode_timeout(
        &mut self,
        program: &str,
        args: &[String],
        environment: &[(&str, &str)],
        mode: CommandMode,
        timeout: Duration,
    ) -> ToolResult<String> {
        mode.check()?;
        self.record(program, args);
        let stdout_name = format!("command-{}.stdout.log", self.commands.len());
        let stderr_name = format!("command-{}.stderr.log", self.commands.len());
        let stdout = self.output.join(&stdout_name);
        let stderr = self.output.join(&stderr_name);
        if let Some(command) = self.commands.last_mut() {
            command["stdout_path"] = json!(stdout_name);
            command["stderr_path"] = json!(stderr_name);
            if !environment.is_empty() {
                command["environment"] = serde_json::to_value(
                    environment
                        .iter()
                        .copied()
                        .collect::<std::collections::BTreeMap<_, _>>(),
                )?;
            }
        }
        let mut child = OwnedCommand(
            Command::new(program)
                .args(args)
                .envs(environment.iter().copied())
                .stdin(Stdio::null())
                .stdout(File::create(&stdout)?)
                .stderr(File::create(&stderr)?)
                .spawn()?,
        );
        let deadline = Instant::now() + timeout;
        let status = loop {
            mode.check()?;
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{program} timed out after {}s; owned child is killed and reaped",
                    timeout.as_secs()
                )
                .into());
            }
            thread::sleep(Duration::from_millis(25));
        };
        let mut text = fs::read_to_string(stdout)?;
        let error = fs::read_to_string(stderr)?;
        if !status.success() {
            return Err(format!("{program} {args:?} failed ({status}): {error}").into());
        }
        text.push_str(&error);
        Ok(text)
    }

    #[cfg(target_os = "macos")]
    fn sample_stall(&mut self, pid: u32, name: &str) {
        // Temporary hosted failure evidence; never a successful profile or retry.
        let path = self.output.join(format!("{name}.sample.txt"));
        let args = [
            pid.to_string(),
            "1".to_owned(),
            "-file".to_owned(),
            path.to_string_lossy().into_owned(),
        ];
        let outcome = self.command_env_mode_timeout(
            "/usr/bin/sample",
            &args,
            &[],
            CommandMode::Cleanup,
            Duration::from_secs(10),
        );
        self.control_messages.push(json!({
            "transport":"temporary-hosted-stall-sample",
            "target_pid":pid,
            "name":name,
            "completed_elapsed_ns":self.started.elapsed().as_nanos(),
            "wall_timeout_seconds":10,
            "status":if outcome.is_ok() {"complete"} else {"failed"},
            "error":outcome.err().map(|error| error.to_string()),
        }));
    }

    fn spawn(&mut self, program: &str, args: &[String]) -> ToolResult<()> {
        crate::scenario::check_cancelled()?;
        self.record(program, args);
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.output.join("profiler.stdout.log"))?,
            )
            .stderr(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.output.join("profiler.stderr.log"))?,
            );
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Own console: CTRL_C cannot reach the LSP's transport or hosted harness.
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            command.creation_flags(CREATE_NEW_CONSOLE);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // The harness cancellation handler controls the collector's one SIGINT.
            command.process_group(0);
        }
        self.child = Some(command.spawn()?);
        Ok(())
    }

    fn wpr(&mut self, args: &[&str]) -> ToolResult<String> {
        self.wpr_mode(args, CommandMode::Active)
    }

    fn wpr_mode(&mut self, args: &[&str], mode: CommandMode) -> ToolResult<String> {
        let mut args: Vec<_> = args.iter().map(|arg| (*arg).to_owned()).collect();
        args.extend(["-instancename".to_owned(), self.instance.clone()]);
        self.command_env_mode("wpr", &args, &[], mode)
    }

    fn prepare_wpr_profile(&mut self) -> ToolResult<()> {
        let name = if self.kind == "heap" { "Heap" } else { "CPU" };
        let source = windows::decoder_path(&self.output.join("native-source.wprp"))?;
        let profile = windows::decoder_path(&self.output.join("native-recording.wprp"))?;
        self.command(
            "wpr",
            &[
                "-exportprofile".to_owned(),
                name.to_owned(),
                source.clone(),
                "-filemode".to_owned(),
            ],
        )?;
        let args = [
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            windows::CONFIGURE_WPR,
        ]
        .map(str::to_owned);
        self.command_env(
            "powershell.exe",
            &args,
            &[
                ("BEND_PERF_WPR_SOURCE", &source),
                ("BEND_PERF_WPR_PROFILE", &profile),
                ("BEND_PERF_WPR_PROFILE_NAME", name),
            ],
        )?;
        self.command(
            "wpr",
            &[
                "-profiledetails".to_owned(),
                format!("{profile}!{name}"),
                "-filemode".to_owned(),
            ],
        )?;
        Ok(())
    }

    fn start_wpr_profile(&mut self) -> ToolResult<()> {
        let name = if self.kind == "heap" { "Heap" } else { "CPU" };
        let profile = windows::decoder_path(&self.output.join("native-recording.wprp"))?;
        self.wpr_active = true;
        self.wpr(&["-start", &format!("{profile}!{name}"), "-filemode"])?;
        Ok(())
    }

    pub(super) fn prepare(&mut self, binary: &Path) -> ToolResult<()> {
        // HeapTracingConfig mutates machine-wide IFEO before execute starts.
        // Install the cooperative handler at this preparation boundary too.
        crate::scenario::install_cancellation_handler()?;
        crate::scenario::check_cancelled()?;
        match self.backend.as_str() {
            "dhat" => {
                self.version = json!("dhat 0.3.3 (embedded allocator; profile format v2)");
            }
            "samply" => {
                let version = self.command("samply", &["--version".to_owned()])?;
                self.tools
                    .push(doctor::tool_identity("samply", version.trim())?);
                self.version = json!(version.trim());
                if cfg!(windows) {
                    let version = self.command("xperf", &["-help".to_owned()])?;
                    self.tools
                        .push(doctor::tool_identity("xperf", version.trim())?);
                }
            }
            "perf" => {
                let version = self.command("perf", &["--version".to_owned()])?;
                self.tools
                    .push(doctor::tool_identity("perf", version.trim())?);
                self.version = json!({"perf": version.trim(), "kernel": self.command("uname", &["-a".to_owned()])?.trim()});
            }
            "xctrace" => {
                let version = self.command("xcodebuild", &["-version".to_owned()])?;
                let sdk = self.command(
                    "xcrun",
                    &[
                        "--sdk".to_owned(),
                        "macosx".to_owned(),
                        "--show-sdk-version".to_owned(),
                    ],
                )?;
                let os = self.command("sw_vers", &[])?;
                let path = self.command("xcrun", &["--find".to_owned(), "xctrace".to_owned()])?;
                self.tools
                    .push(doctor::tool_identity(path.trim(), version.trim())?);
                self.version = json!({"xcode": version.trim(), "sdk": sdk.trim(), "os": os.trim()});
                // Initialize Apple's tracing/authorization services before the
                // cold LSP exists. This trusted target is not scenario evidence;
                // the actual capture still requires its own 60-second start ACK.
                self.command(
                    path.trim(),
                    &[
                        "record".to_owned(),
                        "--template".to_owned(),
                        "Time Profiler".to_owned(),
                        "--time-limit".to_owned(),
                        "1s".to_owned(),
                        "--no-prompt".to_owned(),
                        "--output".to_owned(),
                        self.output
                            .join("tracing-preflight.trace")
                            .to_string_lossy()
                            .into_owned(),
                        "--launch".to_owned(),
                        "--".to_owned(),
                        "/usr/bin/true".to_owned(),
                    ],
                )?;
            }
            "wpr" => {
                let version = self.command("wpr", &["-profiles".to_owned()])?;
                self.tools
                    .push(doctor::tool_identity("wpr", version.trim())?);
                self.version = json!(version.trim());
                self.prepare_wpr_profile()?;
                if self.kind == "heap" {
                    let image = binary
                        .file_name()
                        .and_then(std::ffi::OsStr::to_str)
                        .ok_or("Non-Unicode heap image name")?
                        .to_owned();
                    self.heap_ifeo = Some(self.snapshot_ifeo(&image, CommandMode::Active)?);
                    self.heap_image = Some(image.clone());
                    self.heap_ifeo_restored = Some(false);
                    self.command(
                        "wpr",
                        &["-HeapTracingConfig".to_owned(), image, "enable".to_owned()],
                    )?;
                    // IFEO configuration and Heap session must precede the target process's birth.
                    self.start_wpr_profile()?;
                }
            }
            _ => return Err("Unknown profiler backend".into()),
        }
        Ok(())
    }

    fn snapshot_ifeo(&mut self, image: &str, mode: CommandMode) -> ToolResult<Value> {
        let args = [
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            windows::SNAPSHOT_IFEO,
        ]
        .map(str::to_owned);
        let text = self.command_env_mode(
            "powershell.exe",
            &args,
            &[("BEND_PERF_IMAGE_NAME", image)],
            mode,
        )?;
        let state: Value = serde_json::from_str(text.trim())?;
        windows::validate_snapshot(&state)?;
        Ok(state)
    }

    fn restore_ifeo(&mut self, image: &str) -> ToolResult<()> {
        let state =
            serde_json::to_string(self.heap_ifeo.as_ref().ok_or("Missing prior IFEO state")?)?;
        let args = [
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            windows::RESTORE_IFEO,
        ]
        .map(str::to_owned);
        self.command_env_mode(
            "powershell.exe",
            &args,
            &[
                ("BEND_PERF_IMAGE_NAME", image),
                ("BEND_PERF_IFEO_STATE", &state),
            ],
            CommandMode::Cleanup,
        )?;
        let restored = self.snapshot_ifeo(image, CommandMode::Cleanup)?;
        if self.heap_ifeo.as_ref() != Some(&restored) {
            return Err("IFEO state readback differs from the pre-profile snapshot".into());
        }
        self.heap_ifeo_restored = Some(true);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn start_perf(&mut self, pid: u32) -> ToolResult<()> {
        use nix::{
            fcntl::{OFlag, open},
            sys::stat::Mode,
            unistd::mkfifo,
        };
        let control_path = self.output.join("perf-control.fifo");
        let ack_path = self.output.join("perf-ack.fifo");
        mkfifo(&control_path, Mode::S_IRUSR | Mode::S_IWUSR)?;
        mkfifo(&ack_path, Mode::S_IRUSR | Mode::S_IWUSR)?;
        self.control = Some(File::from(open(
            &control_path,
            OFlag::O_RDWR | OFlag::O_NONBLOCK,
            Mode::empty(),
        )?));
        let mut ack = File::from(open(
            &ack_path,
            OFlag::O_RDWR | OFlag::O_NONBLOCK,
            Mode::empty(),
        )?);
        self.spawn(
            "perf",
            &[
                "record".to_owned(),
                "--pid".to_owned(),
                pid.to_string(),
                "--event".to_owned(),
                "cpu-clock:u".to_owned(),
                "--freq".to_owned(),
                "997".to_owned(),
                "--call-graph".to_owned(),
                "dwarf".to_owned(),
                // perf 6.8 probes hardware cycles for its default ARM64 DWARF
                // register mask, which can include SVE VG. The software clock
                // PMU rejects extended registers with EOPNOTSUPP. Keep every
                // baseline GPR needed for DWARF, without that hardware-only VG.
                #[cfg(target_arch = "aarch64")]
                "--user-regs=x0,x1,x2,x3,x4,x5,x6,x7,x8,x9,x10,x11,x12,x13,x14,x15,x16,x17,x18,x19,x20,x21,x22,x23,x24,x25,x26,x27,x28,x29,lr,sp,pc".to_owned(),
                "--output".to_owned(),
                self.trace.to_string_lossy().into_owned(),
                "--delay=-1".to_owned(),
                format!(
                    "--control=fifo:{},{}",
                    control_path.display(),
                    ack_path.display()
                ),
            ],
        )?;
        self.control_messages.push(json!({"transport": "perf-control-fifo", "payload": "enable\n", "elapsed_ns": self.started.elapsed().as_nanos()}));
        self.control
            .as_mut()
            .ok_or("Missing perf control FIFO")?
            .write_all(b"enable\n")?;
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut response = Vec::new();
        let mut buffer = [0_u8; 64];
        loop {
            crate::scenario::check_cancelled()?;
            match ack.read(&mut buffer) {
                Ok(count) => {
                    response.extend_from_slice(&buffer[..count]);
                    if perf_enable_acknowledged(&response)? {
                        fs::remove_file(control_path)?;
                        fs::remove_file(ack_path)?;
                        return Ok(());
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(status) = self
                .child
                .as_mut()
                .ok_or("Missing perf record process")?
                .try_wait()?
            {
                let mut stderr = Vec::new();
                File::open(self.output.join("profiler.stderr.log"))?
                    .take(8192)
                    .read_to_end(&mut stderr)?;
                return Err(format!(
                    "perf exited before enable acknowledgement: {status}; profiler.stderr.log (first 8192 bytes): {}",
                    String::from_utf8_lossy(&stderr)
                )
                .into());
            }
            if Instant::now() >= deadline {
                return Err("perf did not acknowledge sampling enable within 60s".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn start_perf(_pid: u32) -> ToolResult<()> {
        Err("The perf backend requires native Linux".into())
    }

    fn wait_ready_text(&mut self, marker: &str) -> ToolResult<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut stdout = File::open(self.output.join("profiler.stdout.log"))?;
        let mut stderr = File::open(self.output.join("profiler.stderr.log"))?;
        let mut stdout_evidence = String::new();
        let mut stderr_evidence = String::new();
        loop {
            crate::scenario::check_cancelled()?;
            stdout.read_to_string(&mut stdout_evidence)?;
            stderr.read_to_string(&mut stderr_evidence)?;
            if let Some(status) = self
                .child
                .as_mut()
                .ok_or("Profiler child not running")?
                .try_wait()?
            {
                return Err(
                    format!("Profiler exited before readiness ({status}); stdout: {stdout_evidence}; stderr: {stderr_evidence}").into(),
                );
            }
            if samply_attach_ready(&stderr_evidence, marker) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(
                    format!("Profiler did not report readiness {marker:?}; stdout: {stdout_evidence}; stderr: {stderr_evidence}").into(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn start_xctrace_notifier(&mut self, notification: &str, deadline: Instant) -> ToolResult<()> {
        // Darwin notifications are edges, not retained messages. Process creation
        // does not establish registration. Have notifyutil acknowledge its own
        // registration on a separate key before the recorder can post anything.
        let barrier = format!("{notification}.observer");
        let args = [
            "-z".to_owned(),
            "0".to_owned(),
            "-1".to_owned(),
            notification.to_owned(),
            "-1".to_owned(),
            barrier.clone(),
            "-p".to_owned(),
            barrier.clone(),
        ];
        self.record("notifyutil", &args);
        let stdout_name = "profiler-notifier.stdout.log";
        if let Some(command) = self.commands.last_mut() {
            command["stdout_path"] = json!(stdout_name);
            command["stderr_path"] = json!("profiler.stderr.log");
        }
        self.notifier = Some(
            Command::new("notifyutil")
                .args(&args)
                .stdin(Stdio::null())
                .stdout(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.output.join(stdout_name))?,
                )
                .stderr(
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.output.join("profiler.stderr.log"))?,
                )
                .spawn()?,
        );
        let mut stdout = File::open(self.output.join(stdout_name))?;
        let mut evidence = String::new();
        loop {
            crate::scenario::check_cancelled()?;
            stdout.read_to_string(&mut evidence)?;
            if let Some(status) = self
                .notifier
                .as_mut()
                .ok_or("Missing readiness notifier")?
                .try_wait()?
            {
                return Err(format!(
                    "xctrace notification observer exited before registration ({status}); stdout: {evidence}; stderr: {}",
                    fs::read_to_string(self.output.join("profiler.stderr.log"))?
                )
                .into());
            }
            if notification_received(&evidence, &barrier) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "xctrace notification observer did not acknowledge registration within 60s; stdout: {evidence}; stderr: {}",
                    fs::read_to_string(self.output.join("profiler.stderr.log"))?
                )
                .into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn start_xctrace(&mut self, pid: u32) -> ToolResult<()> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let notification = format!("org.bend2.perf.started.{}.{}", std::process::id(), pid);
        self.start_xctrace_notifier(&notification, deadline)?;
        // Launch the executable whose identity prepare recorded, not an xcrun
        // launcher with an independently owned startup/signal lifetime.
        let program = self
            .tools
            .first()
            .and_then(|tool| tool["path"].as_str())
            .ok_or("Missing prepared xctrace executable identity")?
            .to_owned();
        let template = if self.kind == "heap" {
            "Allocations"
        } else {
            "Time Profiler"
        };
        self.spawn(
            &program,
            &[
                "record".to_owned(),
                "--template".to_owned(),
                template.to_owned(),
                "--attach".to_owned(),
                pid.to_string(),
                "--time-limit".to_owned(),
                "600s".to_owned(),
                "--no-prompt".to_owned(),
                "--notify-tracing-started".to_owned(),
                notification.clone(),
                "--output".to_owned(),
                self.trace.to_string_lossy().into_owned(),
            ],
        )?;
        let mut stdout = File::open(self.output.join("profiler-notifier.stdout.log"))?;
        let mut evidence = String::new();
        loop {
            crate::scenario::check_cancelled()?;
            stdout.read_to_string(&mut evidence)?;
            if let Some(status) = self
                .child
                .as_mut()
                .ok_or("Missing xctrace process")?
                .try_wait()?
            {
                return Err(format!(
                    "xctrace exited before started notification ({status}); stdout: {}; stderr: {}",
                    fs::read_to_string(self.output.join("profiler.stdout.log"))?,
                    fs::read_to_string(self.output.join("profiler.stderr.log"))?
                )
                .into());
            }
            if let Some(status) = self
                .notifier
                .as_mut()
                .ok_or("Missing readiness notifier")?
                .try_wait()?
            {
                stdout.read_to_string(&mut evidence)?;
                if !status.success() || !notification_received(&evidence, &notification) {
                    return Err(format!(
                        "xctrace notification observer failed ({status}); stdout: {evidence}; stderr: {}",
                        fs::read_to_string(self.output.join("profiler.stderr.log"))?
                    )
                    .into());
                }
                self.notifier = None;
                return Ok(());
            }
            if Instant::now() >= deadline {
                #[cfg(target_os = "macos")]
                if let Some(recorder) = self.child.as_ref() {
                    self.sample_stall(recorder.id(), "recorder-readiness-timeout");
                }
                return Err(format!(
                    "xctrace did not post its tracing-started notification within 60s; observer: {evidence}; stdout: {}; stderr: {}",
                    fs::read_to_string(self.output.join("profiler.stdout.log"))?,
                    fs::read_to_string(self.output.join("profiler.stderr.log"))?
                )
                .into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop_child(&mut self, mode: CommandMode) -> ToolResult<()> {
        // Cancellation must not leave a collector waiting out finalization.
        // Cleanup still reaps it, without allowing cancellation to skip work.
        if crate::scenario::check_cancelled().is_err() {
            self.force_reap()?;
            return mode.check();
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let pid = child.id();
        if child.try_wait()?.is_none() {
            let signal = if self.backend == "perf" {
                self.control_messages.push(json!({"transport": "perf-control-fifo", "payload": "stop\n", "elapsed_ns": self.started.elapsed().as_nanos()}));
                match self.control.as_mut() {
                    Some(control) => control.write_all(b"stop\n").map_err(Into::into),
                    None => Err("Missing perf control FIFO".into()),
                }
            } else if self.samply_elevated {
                // The owned child can be sudo's monitor, not the sampler. A
                // successful kill of that monitor is not delivery to samply.
                // Interrupt the unique prepared executable in our owned group.
                Self::elevated_samply_pid(&self.tools, pid).and_then(|sampler_pid| {
                    let args = ["-n", "--", "/bin/kill", "-INT", &sampler_pid.to_string()]
                        .map(str::to_owned);
                    self.cleanup_command("/usr/bin/sudo", &args).map(|_| ())
                })
            } else if cfg!(windows) {
                self.interrupt_windows(pid)
            } else {
                let args = ["-INT".to_owned(), pid.to_string()];
                self.cleanup_command("kill", &args).map(|_| ())
            };
            if let Err(error) = signal {
                self.force_reap()?;
                return Err(
                    format!("Failed to interrupt profiler, forcibly reaped: {error}").into(),
                );
            }
        }
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if let Err(error) = mode.check() {
                self.force_reap()?;
                return Err(error);
            }
            if let Some(status) = self
                .child
                .as_mut()
                .ok_or("Missing profiler child")?
                .try_wait()?
            {
                self.child = None;
                self.control = None;
                self.samply_elevated = false;
                if !status.success() {
                    return Err(format!("Profiler finalization failed: {status}").into());
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                self.force_reap()?;
                return Err("Profiler did not finalize within 300s; killed and reaped".into());
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn elevated_samply_pid(tools: &[Value], group: u32) -> ToolResult<u32> {
        #[cfg(target_os = "macos")]
        {
            let path = tools
                .iter()
                .find(|tool| tool["program"] == "samply")
                .and_then(|tool| tool["path"].as_str())
                .ok_or("Missing prepared samply executable identity")?;
            owned_executable_pid(group, Path::new(path))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (tools, group);
            Err("Elevated samply process ownership is only supported on macOS".into())
        }
    }

    fn force_reap(&mut self) -> ToolResult<()> {
        if self.samply_elevated
            && let Some(child) = self.child.as_ref()
        {
            // Child::kill would only kill sudo, whose SIGKILL cannot be relayed.
            // spawn owns an isolated process group and uses no terminal, so kill
            // that whole group with the same explicitly authorized elevation.
            let group = format!("-{}", child.id());
            let args = ["-n", "--", "/bin/kill", "-KILL", "--", &group].map(str::to_owned);
            if let Err(error) = self.cleanup_command("/usr/bin/sudo", &args)
                && !matches!(self.child.as_mut().map(Child::try_wait), Some(Ok(Some(_))))
            {
                return Err(error);
            }
        }
        if let Some(child) = self.child.as_mut() {
            if child.try_wait()?.is_none() {
                child.kill()?;
            }
            child.wait()?;
            self.child = None;
        }
        self.samply_elevated = false;
        self.control = None;
        Ok(())
    }

    fn interrupt_windows(&mut self, pid: u32) -> ToolResult<()> {
        // CTRL_C can be ignored through an inherited console attribute even
        // after samply installs ctrlc's handler. CTRL_BREAK always invokes that
        // handler. Only the collector's separately created console receives it.
        const SCRIPT: &str = r#"$ErrorActionPreference='Stop'
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; using System.Threading; public static class BendPerfConsole { public delegate bool HandlerRoutine(uint ctrl); public static readonly ManualResetEvent Delivered = new ManualResetEvent(false); public static readonly HandlerRoutine IgnoreBreak = delegate(uint ctrl) { if (ctrl != 1) return false; Delivered.Set(); return true; }; [DllImport("kernel32.dll", SetLastError=true)] public static extern bool FreeConsole(); [DllImport("kernel32.dll", SetLastError=true)] public static extern bool AttachConsole(uint pid); [DllImport("kernel32.dll", SetLastError=true)] public static extern bool SetConsoleCtrlHandler(HandlerRoutine handler, bool add); [DllImport("kernel32.dll", SetLastError=true)] public static extern bool GenerateConsoleCtrlEvent(uint ctrl, uint group); }'
[void][BendPerfConsole]::FreeConsole()
if (![BendPerfConsole]::AttachConsole([uint32]$env:BEND_PERF_COLLECTOR_PID)) { throw 'AttachConsole failed' }
try {
  if (![BendPerfConsole]::SetConsoleCtrlHandler([BendPerfConsole]::IgnoreBreak,$true)) { throw 'SetConsoleCtrlHandler failed' }
  if (![BendPerfConsole]::GenerateConsoleCtrlEvent(1,0)) { throw 'GenerateConsoleCtrlEvent failed' }
  [void][BendPerfConsole]::Delivered.WaitOne()
} finally {
  [void][BendPerfConsole]::FreeConsole()
}"#;
        let args = ["-NoProfile", "-NonInteractive", "-Command", SCRIPT].map(str::to_owned);
        let pid = pid.to_string();
        self.command_env_mode(
            "powershell.exe",
            &args,
            &[("BEND_PERF_COLLECTOR_PID", &pid)],
            CommandMode::Cleanup,
        )?;
        Ok(())
    }

    fn cleanup_samply_etw(&mut self) -> ToolResult<()> {
        // A forcibly terminated sampler can orphan its hidden elevated helper.
        // Match this unique output path, never another recording's helper.
        const SCRIPT: &str = r"$ErrorActionPreference='Stop'
$helpers = @(Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'samply.exe' -and $_.CommandLine -and $_.CommandLine.Contains('run-elevated-helper') -and $_.CommandLine.Contains($env:BEND_PERF_TRACE_PATH) })
foreach ($helper in $helpers) {
  $process = Get-Process -Id $helper.ProcessId -ErrorAction SilentlyContinue
  if ($process) { $process.Kill(); if (!$process.WaitForExit(30000)) { throw 'Owned sampler helper did not exit after termination' } }
}";
        if !self.samply_etw_owned {
            return Ok(());
        }
        let loggers = self.cleanup_command("xperf", &["-loggers".to_owned()])?;
        if doctor::kernel_logger_running(&loggers) {
            self.cleanup_command("xperf", &["-stop".to_owned()])?;
            let remaining = self.cleanup_command("xperf", &["-loggers".to_owned()])?;
            if doctor::kernel_logger_running(&remaining) {
                return Err("Owned samply kernel ETW session remained active after stop".into());
            }
        }
        let trace = self.trace.to_string_lossy().into_owned();
        let args = ["-NoProfile", "-NonInteractive", "-Command", SCRIPT].map(str::to_owned);
        self.command_env_mode(
            "powershell.exe",
            &args,
            &[("BEND_PERF_TRACE_PATH", &trace)],
            CommandMode::Cleanup,
        )?;
        self.samply_etw_owned = false;
        Ok(())
    }

    fn stop_wpr(&mut self) -> ToolResult<()> {
        if self.wpr_active {
            let path = self.trace.to_string_lossy().into_owned();
            let result = self.wpr_mode(
                &["-stop", &path, "Bend2 LSP isolated semantic scenario"],
                CommandMode::Cleanup,
            )?;
            self.wpr_active = false;
            windows::validate_wpr_stop(&result)?;
        }
        Ok(())
    }

    pub(super) fn profiler(&self) -> Value {
        let viewer = match self.backend.as_str() {
            "samply" => vec![
                "samply",
                "load",
                "samply.json",
                "--symbol-dir",
                "symbols",
                "--address",
                "127.0.0.1",
            ],
            "perf" => vec!["perf", "report", "--input", "perf.data"],
            "xctrace" => vec!["open", "-a", "Instruments", "native.trace"],
            "wpr" => vec!["wpa.exe", "native.etl"],
            _ => Vec::new(),
        };
        json!({"version": self.version, "commands": self.commands, "control_messages": self.control_messages, "tools": self.tools,
            "heap_ifeo_state_before": self.heap_ifeo, "heap_ifeo_restored": self.heap_ifeo_restored,
            "source": if self.backend == "samply" { doctor::samply_source() } else { "OS/Xcode/kernel tool identity pinned by version and binary SHA256" },
            "viewer_command": viewer, "viewer_url": if self.backend == "dhat" {Some("https://nnethercote.github.io/dh_view/dh_view.html")} else {None},
            "target_environment": if self.backend == "xctrace" && self.kind == "heap" {
                json!({"MallocNanoZone": "0"})
            } else {json!({})},
            "phase_markers": if self.backend == "wpr" {"ETW wpr -marker and manifest timestamps"} else {"manifest timestamps; no fabricated in-trace marker support"}})
    }

    pub(super) fn validate(&mut self, binary: &Path) -> ToolResult<()> {
        if self.backend == "dhat" {
            self.heap_summary = crate::native::validate_dhat_profile(
                &self.trace,
                self.pid.ok_or("Missing DHAT target PID")?,
                &[binary.to_str().ok_or("Non-Unicode binary path")?.to_owned()],
            )?;
        } else if self.backend == "samply" {
            let data: Value = serde_json::from_reader(File::open(&self.trace)?)?;
            let pid = self.pid.ok_or("Missing sampler target PID")?;
            if !has_samples(&data, pid) {
                return Err("Samply profile has no actual target-PID CPU samples".into());
            }
            if cfg!(windows) {
                self.validate_etl()?;
            }
        } else if self.backend == "perf" {
            let trace = self.trace.to_string_lossy().into_owned();
            let samples = self.command(
                "perf",
                &[
                    "script".to_owned(),
                    "--input".to_owned(),
                    trace,
                    "--fields".to_owned(),
                    "pid".to_owned(),
                ],
            )?;
            let pid = self.pid.ok_or("Missing perf target PID")?.to_string();
            if !samples.lines().any(|line| line.trim() == pid) {
                return Err("perf.data has no actual target-PID sample records".into());
            }
        } else if self.backend == "xctrace" {
            self.validate_instruments()?;
        } else {
            self.validate_etl()?;
        }
        Ok(())
    }

    fn validate_etl(&mut self) -> ToolResult<()> {
        let path = self.output.join("native-events.xml");
        let trace = if self.backend == "samply" {
            self.trace.with_extension("kernel.etl")
        } else {
            self.trace.clone()
        };
        let args = [
            windows::decoder_path(&trace)?,
            windows::decoder_path(&path)?,
            self.pid.ok_or("Missing ETW target PID")?.to_string(),
            self.kind.clone(),
        ];
        self.command("bend2-etl-reader", &args)?;
        self.tools.push(doctor::tool_identity(
            "bend2-etl-reader",
            doctor::ETL_READER_VERSION,
        )?);
        let summary = validation::etl_summary(
            &path,
            self.pid.ok_or("Missing ETW target PID")?,
            self.kind == "heap",
        )?;
        crate::common::write_json(&self.output.join("native-validation.json"), &summary)?;
        if self.kind == "heap" {
            self.heap_summary = summary;
        }
        Ok(())
    }

    fn validate_instruments(&mut self) -> ToolResult<()> {
        let toc = self.command(
            "xcrun",
            &[
                "xctrace".to_owned(),
                "export".to_owned(),
                "--input".to_owned(),
                self.trace.to_string_lossy().into_owned(),
                "--toc".to_owned(),
            ],
        )?;
        fs::write(self.output.join("native-toc.xml"), &toc)?;
        let heap = self.kind == "heap";
        let entities = validation::instrument_entities(
            &toc,
            self.pid.ok_or("Missing Instruments target PID")?,
            heap,
        )?;
        let entity = entities
            .into_iter()
            .next()
            .ok_or("Missing instrument table")?;
        let path = self.output.join("native-table.xml");
        self.command(
            "xcrun",
            &[
                "xctrace".to_owned(),
                "export".to_owned(),
                "--input".to_owned(),
                self.trace.to_string_lossy().into_owned(),
                "--xpath".to_owned(),
                entity.xpath.clone(),
                "--output".to_owned(),
                path.to_string_lossy().into_owned(),
            ],
        )?;
        let rows = validation::instrument_rows(
            &path,
            self.pid.ok_or("Missing Instruments target PID")?,
            heap,
            Some(&entity),
        )?;
        if rows == 0 {
            return Err("Instruments exported no actual CPU sample / allocation rows".into());
        }
        let summary = json!({"target_pid": self.pid, "schema_or_detail": entity.label, "recorded_rows": rows, "measurement": "Original Instruments CPU/allocator diagnostic rows, not RSS or clean latency"});
        crate::common::write_json(&self.output.join("native-validation.json"), &summary)?;
        if heap {
            self.heap_summary = summary;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn owned_executable_pid(group: u32, executable: &Path) -> ToolResult<u32> {
    use libproc::libproc::proc_pid::pidpath;
    use libproc::processes::{ProcFilter, pids_by_type};

    let executable = fs::canonicalize(executable)?;
    let mut owned_pid = None;
    for pid in pids_by_type(ProcFilter::ByProgramGroup { pgrpid: group })? {
        let path = pidpath(i32::try_from(pid)?)
            .map_err(|error| format!("Cannot inspect owned process {pid}: {error}"))?;
        if Path::new(&path) == executable && owned_pid.replace(pid).is_some() {
            return Err(format!(
                "Multiple instances of {} in owned process group {group}",
                executable.display()
            )
            .into());
        }
    }
    owned_pid.ok_or_else(|| {
        format!(
            "No instance of {} in owned process group {group}",
            executable.display()
        )
        .into()
    })
}

fn notification_received(stdout: &str, key: &str) -> bool {
    stdout
        .split_inclusive('\n')
        .any(|line| line.strip_suffix('\n') == Some(key))
}

fn samply_elevation_requested(
    os: &str,
    configured: Option<&str>,
    github_actions: Option<&str>,
) -> ToolResult<bool> {
    if os != "macos" || configured != Some("true") {
        return Ok(false);
    }
    if github_actions != Some("true") {
        return Err(
            "Explicit macOS samply elevation is restricted to hosted GitHub Actions".into(),
        );
    }
    Ok(true)
}

fn samply_attach_ready(stderr: &str, marker: &str) -> bool {
    // Pinned samply 0.13.1 emits a complete PID-bound stderr line after obtaining
    // the macOS root Mach task (Linux: initialized perf events; Windows: started xperf).
    // A partial line, stdout echo, or banner for another PID is not an attach ACK.
    let Some(marker) = marker.strip_suffix('\n') else {
        return false;
    };
    stderr.split_inclusive('\n').any(|line| {
        line.strip_suffix('\n')
            .map(|line| match line.strip_suffix('\r') {
                Some(native_line) => native_line,
                None => line,
            })
            == Some(marker)
    })
}

#[cfg(any(target_os = "linux", test))]
fn perf_enable_acknowledged(response: &[u8]) -> ToolResult<bool> {
    // perf's evlist__ctlfd_ack writes sizeof("ack\n"), including the C NUL.
    // A fragmented newline is not yet a complete frame; no other bytes are valid.
    const ACK: &[u8] = b"ack\n\0";
    if !ACK.starts_with(response) {
        return Err(format!("perf enable rejected: {response:?}").into());
    }
    Ok(response.len() == ACK.len())
}

pub(super) fn has_samples(data: &Value, pid: u32) -> bool {
    let pid_text = pid.to_string();
    data["threads"].as_array().is_some_and(|threads| {
        threads.iter().any(|thread| {
            if thread["pid"].as_u64() != Some(u64::from(pid))
                && thread["pid"].as_str() != Some(pid_text.as_str())
            {
                return false;
            }
            let samples = &thread["samples"];
            let (Some(count), Some(stacks), Some(weights), Some(deltas)) = (
                samples["length"].as_u64(),
                samples["stack"].as_array(),
                samples["weight"].as_array(),
                samples["threadCPUDelta"].as_array(),
            ) else {
                return false;
            };
            count > 0
                && [stacks.len(), weights.len(), deltas.len()]
                    .into_iter()
                    .all(|length| u64::try_from(length).ok() == Some(count))
                && stacks
                    .iter()
                    .zip(weights)
                    .zip(deltas)
                    .any(|((stack, weight), delta)| {
                        stack.as_u64().is_some()
                            && weight.as_f64().is_some_and(|value| value > 0.0)
                            && delta.as_f64().is_some_and(|value| value > 0.0)
                    })
        })
    })
}

impl ScenarioSession for Session {
    fn child_environment(&self) -> Vec<(OsString, OsString)> {
        if self.backend == "dhat" {
            vec![(
                OsString::from("BEND2_LSP_DHAT_FILE"),
                self.trace.as_os_str().to_owned(),
            )]
        } else if self.backend == "xctrace" && self.kind == "heap" {
            // Nano's in-process heap enumerator allocates from the helper zone.
            // Trace only the scalable allocator to avoid that attach interaction.
            // This compatibility setting is not default-process memory evidence.
            vec![(OsString::from("MallocNanoZone"), OsString::from("0"))]
        } else {
            Vec::new()
        }
    }
    fn finish_before_shutdown(&self) -> bool {
        self.backend != "dhat"
    }
    fn finalization_timeout(&self) -> Duration {
        Duration::from_secs(300)
    }
    fn started(&mut self, pid: u32) -> ToolResult<()> {
        crate::scenario::check_cancelled()?;
        self.pid = Some(pid);
        match self.backend.as_str() {
            "dhat" => {}
            "samply" => {
                let mut args = vec![
                    "record".to_owned(),
                    "--save-only".to_owned(),
                    "--pid".to_owned(),
                    pid.to_string(),
                    "--output".to_owned(),
                    self.trace.to_string_lossy().into_owned(),
                ];
                if cfg!(windows) {
                    args.push("--keep-etl".to_owned());
                }
                let elevated = samply_elevation_requested(
                    std::env::consts::OS,
                    std::env::var("BEND_PERF_MACOS_SAMPLY_ELEVATED")
                        .ok()
                        .as_deref(),
                    std::env::var("GITHUB_ACTIONS").ok().as_deref(),
                )?;
                if elevated {
                    // sudo can allocate a separate pty/process group if /dev/tty
                    // opens, even when standard streams are files. Hosted-only
                    // elevation must remain in our isolated owned process group.
                    match OpenOptions::new().read(true).write(true).open("/dev/tty") {
                        // Darwin ENXIO: this process has no controlling terminal.
                        Err(error) if error.raw_os_error() == Some(6) => {}
                        Err(error) => {
                            return Err(format!(
                                "Cannot establish headless macOS samply elevation: {error}"
                            )
                            .into());
                        }
                        Ok(_) => {
                            return Err(
                                "Explicit macOS samply elevation requires no controlling terminal"
                                    .into(),
                            );
                        }
                    }
                    let path = self
                        .tools
                        .iter()
                        .find(|tool| tool["program"] == "samply")
                        .and_then(|tool| tool["path"].as_str())
                        .ok_or("Missing prepared samply executable identity for elevation")?;
                    let mut sudo_args = vec!["-n".to_owned(), "--".to_owned(), path.to_owned()];
                    sudo_args.extend(args);
                    self.samply_elevated = true;
                    self.spawn("/usr/bin/sudo", &sudo_args)?;
                } else {
                    self.spawn("samply", &args)?;
                }
                self.samply_etw_owned = cfg!(windows);
                let marker = match std::env::consts::OS {
                    "linux" => format!("Recording process with PID {pid} until Ctrl+C...\n"),
                    "macos" => format!("Profiling {pid}, press Ctrl-C to stop...\n"),
                    _ => format!("Profiling process with pid {pid}...\n"),
                };
                self.wait_ready_text(&marker)?;
                if cfg!(windows) {
                    let loggers = self.command("xperf", &["-loggers".to_owned()])?;
                    if !doctor::kernel_logger_running(&loggers) {
                        return Err("Samply did not start its kernel ETW session; the upstream banner alone is not readiness".into());
                    }
                }
            }
            "perf" => {
                #[cfg(target_os = "linux")]
                self.start_perf(pid)?;
                #[cfg(not(target_os = "linux"))]
                Self::start_perf(pid)?;
            }
            "xctrace" => self.start_xctrace(pid)?,
            "wpr" => {
                if !self.wpr_active {
                    self.start_wpr_profile()?;
                }
                let status = self.wpr(&["-status"])?;
                if !status.contains("recording is in progress") {
                    return Err(format!("WPR session failed readiness: {status}").into());
                }
                self.wpr(&["-status", "collectors", "-details"])?;
            }
            _ => return Err("Unknown backend".into()),
        }
        self.phase("profiler.ready")
    }
    fn phase(&mut self, name: &str) -> ToolResult<()> {
        crate::scenario::check_cancelled()?;
        self.phases
            .push(json!({"name": name, "elapsed_ns": self.started.elapsed().as_nanos()}));
        #[cfg(target_os = "macos")]
        if self.backend == "xctrace"
            && self.kind == "heap"
            && name == "request_timeout.before_abort"
            && let Some(pid) = self.pid
        {
            self.sample_stall(pid, "request-timeout");
        }
        if self.backend == "wpr" && self.wpr_active {
            self.wpr(&["-marker", name])?;
        }
        Ok(())
    }
    fn finished(&mut self) -> ToolResult<()> {
        self.stop_child(CommandMode::Active)?;
        self.stop_wpr()?;
        self.cleanup_samply_etw()?;
        crate::scenario::check_cancelled()?;
        Ok(())
    }
    fn abort(&mut self) -> ToolResult<()> {
        let mut errors = Vec::new();
        if let Some(mut notifier) = self.notifier.take() {
            if let Err(error) = notifier.kill() {
                errors.push(error.to_string());
            }
            if let Err(error) = notifier.wait() {
                errors.push(error.to_string());
            }
        }
        if let Err(error) = self.stop_child(CommandMode::Cleanup) {
            errors.push(error.to_string());
        }
        if let Err(error) = self.cleanup_samply_etw() {
            errors.push(error.to_string());
        }
        if self.wpr_active {
            match self.wpr_mode(&["-cancel"], CommandMode::Cleanup) {
                Ok(_) => self.wpr_active = false,
                Err(error) => errors.push(error.to_string()),
            }
        }
        if let Some(image) = self.heap_image.clone() {
            let args = [
                "-HeapTracingConfig".to_owned(),
                image.clone(),
                "disable".to_owned(),
            ];
            if let Err(error) = self.cleanup_command("wpr", &args) {
                errors.push(error.to_string());
            }
            // Restore even if WPR's disable failed; retain state for Drop retry on failure.
            match self.restore_ifeo(&image) {
                Ok(()) => self.heap_image = None,
                Err(error) => errors.push(error.to_string()),
            }
        }
        for name in ["perf-control.fifo", "perf-ack.fifo"] {
            if let Err(error) = fs::remove_file(self.output.join(name))
                && error.kind() != std::io::ErrorKind::NotFound
            {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; ").into())
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.abort();
    }
}

struct OwnedCommand(Child);

impl Drop for OwnedCommand {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        has_samples, perf_enable_acknowledged, samply_attach_ready, samply_elevation_requested,
    };
    use serde_json::{Value, json};

    #[test]
    fn macos_samply_elevation_requires_explicit_hosted_configuration() -> super::ToolResult<()> {
        assert!(samply_elevation_requested(
            "macos",
            Some("true"),
            Some("true")
        )?);
        for configuration in [None, Some("false"), Some("1"), Some("TRUE")] {
            assert!(!samply_elevation_requested(
                "macos",
                configuration,
                Some("true")
            )?);
        }
        for hosted in [None, Some("false"), Some("1")] {
            assert!(samply_elevation_requested("macos", Some("true"), hosted).is_err());
        }
        for os in ["linux", "windows"] {
            assert!(!samply_elevation_requested(os, Some("true"), Some("true"))?);
        }
        Ok(())
    }

    #[test]
    fn notification_requires_a_complete_exact_key() {
        let key = "org.bend2.perf.started.17357.17358";
        let event = format!("{key}\n");
        assert!(super::notification_received(&event, key));
        assert!(super::notification_received(
            &format!("{key}.observer\n{event}"),
            key
        ));
        for end in 0..event.len() {
            assert!(!super::notification_received(&event[..end], key));
        }
        for stdout in [
            format!("{key}.observer\n"),
            format!("{key}0\n"),
            format!("{key}: Failed with code 1\n"),
        ] {
            assert!(!super::notification_received(&stdout, key), "{stdout:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn notification_observer_registers_before_the_recorder_can_post() -> super::ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let key = format!(
            "org.bend2.perf.test.{}.{}",
            std::process::id(),
            directory.path().display()
        );
        let mut session = super::Session::new("xctrace", "cpu", directory.path());
        let deadline = super::Instant::now() + super::Duration::from_secs(10);
        session.start_xctrace_notifier(&key, deadline)?;
        let path = directory.path().join("profiler-notifier.stdout.log");
        let registered = super::fs::read_to_string(&path)?;
        assert!(super::notification_received(
            &registered,
            &format!("{key}.observer")
        ));
        assert!(!super::notification_received(&registered, &key));
        assert!(
            session
                .notifier
                .as_mut()
                .ok_or("Missing test observer")?
                .try_wait()?
                .is_none()
        );
        // Post immediately after the registration ACK; no timing sleep bridges
        // a lost-edge race, and no Instruments measurement is collected.
        assert!(
            std::process::Command::new("notifyutil")
                .args(["-z", "0", "-p", &key])
                .status()?
                .success()
        );
        loop {
            if let Some(status) = session
                .notifier
                .as_mut()
                .ok_or("Missing test observer")?
                .try_wait()?
            {
                session.notifier = None;
                assert!(status.success(), "{status}");
                break;
            }
            assert!(super::Instant::now() < deadline, "Notification was lost");
            super::thread::sleep(super::Duration::from_millis(10));
        }
        assert!(super::notification_received(
            &super::fs::read_to_string(path)?,
            &key
        ));
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sampler_interrupt_selects_the_executable_not_its_monitor() -> super::ToolResult<()> {
        use std::os::unix::process::CommandExt;

        let monitor = super::OwnedCommand(
            std::process::Command::new("/bin/sh")
                .args(["-c", "read -r line"])
                .stdin(std::process::Stdio::piped())
                .process_group(0)
                .spawn()?,
        );
        let group = monitor.0.id();
        let sampler = super::OwnedCommand(
            std::process::Command::new("/bin/sleep")
                .arg("300")
                .process_group(i32::try_from(group)?)
                .spawn()?,
        );
        let mut unrelated = super::OwnedCommand(
            std::process::Command::new("/bin/sleep")
                .arg("300")
                .process_group(0)
                .spawn()?,
        );
        assert_eq!(
            super::owned_executable_pid(group, std::path::Path::new("/bin/sleep"))?,
            sampler.0.id()
        );
        assert_ne!(sampler.0.id(), group);
        assert!(unrelated.0.try_wait()?.is_none());
        let _ambiguous = super::OwnedCommand(
            std::process::Command::new("/bin/sleep")
                .arg("300")
                .process_group(i32::try_from(group)?)
                .spawn()?,
        );
        assert!(
            super::owned_executable_pid(group, std::path::Path::new("/bin/sleep")).is_err(),
            "Do not guess which of two matching owned executables to interrupt"
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn collector_interrupt_reaches_a_console_ignoring_ctrl_c() -> super::ToolResult<()> {
        // Exercise the actual console APIs with an owned waiting process, not
        // a fabricated profiler trace or an assertion about a command string.
        const SCRIPT: &str = r#"$ErrorActionPreference='Stop'
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; using System.Threading; public static class IgnoringConsole { public delegate bool HandlerRoutine(uint ctrl); public static readonly ManualResetEvent Stopped = new ManualResetEvent(false); public static readonly HandlerRoutine Stop = delegate(uint ctrl) { if (ctrl != 1) return false; Stopped.Set(); return true; }; [DllImport("kernel32.dll", EntryPoint="SetConsoleCtrlHandler", SetLastError=true)] public static extern bool Ignore(IntPtr handler, bool add); [DllImport("kernel32.dll", EntryPoint="SetConsoleCtrlHandler", SetLastError=true)] public static extern bool Register(HandlerRoutine handler, bool add); }'
if (![IgnoringConsole]::Ignore([IntPtr]::Zero,$true)) { throw 'Could not ignore CTRL_C' }
if (![IgnoringConsole]::Register([IgnoringConsole]::Stop,$true)) { throw 'Could not register CTRL_BREAK handler' }
[Console]::Error.WriteLine('console.fixture.ready')
[void][IgnoringConsole]::Stopped.WaitOne()
"#;
        let directory = tempfile::tempdir()?;
        let mut session = super::Session::new("samply", "cpu", directory.path());
        let args = ["-NoProfile", "-NonInteractive", "-Command", SCRIPT].map(str::to_owned);
        session.spawn("powershell.exe", &args)?;
        session.wait_ready_text("console.fixture.ready\n")?;
        let pid = session
            .child
            .as_ref()
            .ok_or("Missing console fixture")?
            .id();
        session.interrupt_windows(pid)?;
        let deadline = super::Instant::now() + super::Duration::from_secs(10);
        let completed = loop {
            if let Some(status) = session
                .child
                .as_mut()
                .ok_or("Missing console fixture")?
                .try_wait()?
            {
                break status.success();
            }
            if super::Instant::now() >= deadline {
                break false;
            }
            super::thread::sleep(super::Duration::from_millis(10));
        };
        session.force_reap()?;
        assert!(
            completed,
            "CTRL_C ignore must not block collector finalization"
        );
        Ok(())
    }

    #[test]
    fn perf_ack_requires_the_upstream_nul_terminated_frame() -> super::ToolResult<()> {
        let hosted_ack = [97, 99, 107, 10, 0];
        assert!(perf_enable_acknowledged(&hosted_ack)?);
        // Every possible FIFO split must wait for the complete sizeof("ack\n") write.
        for split in 0..hosted_ack.len() {
            assert!(!perf_enable_acknowledged(&hosted_ack[..split])?);
            let mut response = hosted_ack[..split].to_vec();
            response.extend_from_slice(&hosted_ack[split..]);
            assert!(perf_enable_acknowledged(&response)?);
        }
        Ok(())
    }

    #[test]
    fn perf_ack_rejects_malformed_or_extra_frames() {
        for response in [
            b"nak\n\0".as_slice(),
            b"ack\0",
            b"ack\r\n\0",
            b"ack\n\n",
            b"ack\n\0\0",
            b"ack\n\0ack\n\0",
            b"ack\n\0garbage",
        ] {
            assert!(
                perf_enable_acknowledged(response).is_err(),
                "Accepted malformed ACK {response:?}"
            );
        }
    }

    #[test]
    fn samply_attach_requires_the_complete_pid_bound_stderr_banner() {
        // ExistingProcessRunner::run_root_task in the pinned da75c28 source.
        let marker = "Profiling 17357, press Ctrl-C to stop...\n";
        assert!(samply_attach_ready(marker, marker));
        assert!(samply_attach_ready(
            &format!("Warning: child task already exited.\n{marker}"),
            marker
        ));
        for end in 0..marker.len() {
            assert!(!samply_attach_ready(&marker[..end], marker));
        }
        for stderr in [
            "",
            "Profiling 17358, press Ctrl-C to stop...\n",
            "Profiling 173570, press Ctrl-C to stop...\n",
            "error: Profiling 17357, press Ctrl-C to stop...\n",
            "Profiling 17357, press Ctrl-C to stop...not attached\n",
            "Error: task_for_pid for target task failed with error code 5.\n",
            "Code signing successful!\n",
        ] {
            assert!(!samply_attach_ready(stderr, marker), "{stderr:?}");
        }
    }

    #[test]
    fn native_console_readiness_requires_the_complete_crlf_banner() {
        let marker = "console.fixture.ready\n";
        let banner = "console.fixture.ready\r\n";
        assert!(samply_attach_ready(banner, marker));
        assert!(samply_attach_ready(
            &format!("earlier line\r\n{banner}"),
            marker
        ));
        for end in 0..banner.len() {
            assert!(!samply_attach_ready(&banner[..end], marker));
        }
        for stderr in [
            "console.fixture.ready\r",
            "console.fixture.ready\r\r\n",
            "console.fixture.ready.extra\r\n",
            "error: console.fixture.ready\r\n",
        ] {
            assert!(!samply_attach_ready(stderr, marker), "{stderr:?}");
        }
        assert!(!samply_attach_ready(banner, "console.fixture.ready"));
    }

    #[cfg(unix)]
    #[test]
    fn exited_samply_banner_cannot_acknowledge_attachment() -> super::ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let marker = "Profiling 17357, press Ctrl-C to stop...\n";
        super::fs::write(directory.path().join("profiler.stdout.log"), "")?;
        super::fs::write(directory.path().join("profiler.stderr.log"), marker)?;
        let mut session = super::Session::new("samply", "cpu", directory.path());
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()?;
        child.wait()?;
        session.child = Some(child);
        let error = session
            .wait_ready_text(marker)
            .expect_err("An exited recorder is not ready even with a valid banner");
        assert!(
            error.to_string().contains("exited before readiness"),
            "{error}"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn samply_stdout_banner_is_not_an_attach_ack() -> super::ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let marker = "Profiling 17357, press Ctrl-C to stop...\n";
        super::fs::write(directory.path().join("profiler.stdout.log"), marker)?;
        // Real hosted run 38071554112 had empty logs; empty stderr is never ready.
        super::fs::write(directory.path().join("profiler.stderr.log"), "")?;
        let mut session = super::Session::new("samply", "cpu", directory.path());
        session.child = Some(
            std::process::Command::new("/bin/sleep")
                .arg("0.1")
                .spawn()?,
        );
        let error = session
            .wait_ready_text(marker)
            .expect_err("A stdout echo must not unblock the workload");
        assert!(
            error.to_string().contains("exited before readiness"),
            "{error}"
        );
        Ok(())
    }

    fn profile(pid: &Value) -> Value {
        json!({"threads": [{"pid": pid, "samples": {
            "length": 1, "stack": [0], "weight": [1], "threadCPUDelta": [125.0]
        }}]})
    }

    #[test]
    fn cpu_evidence_requires_actual_target_pid() {
        assert!(has_samples(&profile(&json!(123)), 123));
        assert!(has_samples(&profile(&json!("123")), 123));
        assert!(!has_samples(&profile(&json!(124)), 123));
    }

    #[test]
    fn cpu_evidence_rejects_idle_or_metadata_only_samples() {
        let mut data = profile(&json!(123));
        data["threads"][0]["samples"]["threadCPUDelta"] = json!([0]);
        assert!(!has_samples(&data, 123));
        data["threads"][0]["samples"]["threadCPUDelta"] = json!([125]);
        data["threads"][0]["samples"]["stack"] = json!([null]);
        assert!(!has_samples(&data, 123));
    }

    #[test]
    fn cpu_evidence_rejects_incoherent_columns() {
        let mut data = profile(&json!(123));
        data["threads"][0]["samples"]["length"] = json!(2);
        assert!(!has_samples(&data, 123));
        data["threads"][0]["samples"]["length"] = json!(1);
        data["threads"][0]["samples"]["weight"] = json!([]);
        assert!(!has_samples(&data, 123));
    }

    #[cfg(unix)]
    fn wait_for_cancellation_ready(
        child: &mut std::process::Child,
        ready: &std::path::Path,
        mode: &str,
    ) -> super::ToolResult<()> {
        use super::{Duration, Instant, thread};
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                child.try_wait()?.is_none(),
                "Cancellation fixture exited before reaching {mode}"
            );
            assert!(
                Instant::now() < deadline,
                "Cancellation fixture never reached {mode}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn cancellation_case(mode: &str, signal: &str) -> super::ToolResult<()> {
        use std::{
            fs,
            os::unix::fs::PermissionsExt,
            process::{Command, Stdio},
            thread,
            time::{Duration, Instant},
        };
        struct FixtureCleanup {
            pid_file: std::path::PathBuf,
            active: bool,
        }
        impl Drop for FixtureCleanup {
            fn drop(&mut self) {
                if self.active
                    && let Ok(pid) = fs::read_to_string(&self.pid_file)
                {
                    let _ = Command::new("/bin/kill")
                        .args(["-KILL", &pid])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
        let directory = tempfile::tempdir()?;
        let collector = directory.path().join("samply");
        // A real blocked collector, not synthetic profiler-success evidence.
        fs::write(
            &collector,
            "#!/bin/sh\ntrap '' INT\nprintf '%s' \"$$\" > \"$BEND_PERF_CANCELLATION_DIR/collector.pid\"\nexec /bin/sleep 30\n",
        )?;
        fs::set_permissions(&collector, fs::Permissions::from_mode(0o700))?;
        // Keep failing-before regressions from orphaning their blocked fixture.
        let mut cleanup = FixtureCleanup {
            pid_file: directory.path().join("collector.pid"),
            active: true,
        };
        let mut child = super::OwnedCommand(
            Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "profiling::sessions::tests::cancellation_fixture",
                    "--nocapture",
                ])
                .env("BEND_PERF_CANCELLATION_CASE", mode)
                .env("BEND_PERF_CANCELLATION_DIR", directory.path())
                .env_remove("BEND_PERF_MACOS_SAMPLY_ELEVATED")
                .env("PATH", directory.path())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        let ready = directory.path().join(if mode == "prepare-signal" {
            "prepared"
        } else {
            "collector.pid"
        });
        wait_for_cancellation_ready(&mut child.0, &ready, mode)?;
        let started = Instant::now();
        assert!(
            Command::new("/bin/kill")
                .args([signal, &child.0.id().to_string()])
                .status()?
                .success()
        );
        let status = loop {
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "Cancellation did not unblock {mode} promptly"
            );
            thread::sleep(Duration::from_millis(10));
        };
        let mut evidence = String::new();
        if let Some(mut stdout) = child.0.stdout.take() {
            std::io::Read::read_to_string(&mut stdout, &mut evidence)?;
        }
        if let Some(mut stderr) = child.0.stderr.take() {
            std::io::Read::read_to_string(&mut stderr, &mut evidence)?;
        }
        assert!(
            status.success(),
            "{mode} cancellation failed: {status}: {evidence}"
        );
        if mode != "prepare-signal" {
            let pid = fs::read_to_string(directory.path().join("collector.pid"))?;
            assert!(
                !Command::new("/bin/kill")
                    .args(["-0", &pid])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()?
                    .success(),
                "Owned collector remained alive after {mode} cancellation"
            );
        }
        cleanup.active = false;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn preparation_installs_cleanup_capable_signal_handler() -> super::ToolResult<()> {
        cancellation_case("prepare-signal", "-TERM")
    }

    #[cfg(unix)]
    #[test]
    fn blocked_preparation_command_is_cancelled_and_reaped() -> super::ToolResult<()> {
        cancellation_case("command", "-INT")
    }

    #[cfg(unix)]
    #[test]
    fn blocked_recorder_readiness_is_cancelled_and_reaped() -> super::ToolResult<()> {
        cancellation_case("readiness", "-TERM")
    }

    #[cfg(unix)]
    #[test]
    fn cancellation_fixture() -> super::ToolResult<()> {
        use super::{ScenarioSession, Session};
        use std::{
            fs,
            path::Path,
            thread,
            time::{Duration, Instant},
        };
        let Ok(mode) = std::env::var("BEND_PERF_CANCELLATION_CASE") else {
            return Ok(());
        };
        let directory = std::path::PathBuf::from(
            std::env::var_os("BEND_PERF_CANCELLATION_DIR").ok_or("Missing fixture directory")?,
        );
        let mut session = Session::new("dhat", "heap", &directory);
        session.prepare(Path::new("not-spawned"))?;
        if mode == "prepare-signal" {
            fs::write(directory.join("prepared"), b"")?;
            let deadline = Instant::now() + Duration::from_secs(5);
            // Observe cancellation through the collector hook, not handler internals.
            loop {
                if let Err(error) = session.started(123) {
                    assert!(error.to_string().contains("cancelled"), "{error}");
                    session.abort()?;
                    return Ok(());
                }
                assert!(
                    Instant::now() < deadline,
                    "Preparation did not install a cooperative handler"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
        let mut session = Session::new("samply", "cpu", &directory);
        let outcome = if mode == "command" {
            session.prepare(Path::new("not-spawned"))
        } else {
            session.started(123)
        };
        let error = outcome.expect_err("Blocked collector unexpectedly succeeded");
        assert!(error.to_string().contains("cancelled"), "{error}");
        // Drop must clean up even when the caller does not explicitly abort.
        drop(session);
        Ok(())
    }
}
