//! Bounded collection supervision: a reaped leader alone is not a stopped collection.

use crate::ToolResult;

pub(super) fn run(args: &[String]) -> ToolResult<()> {
    #[cfg(unix)]
    {
        unix::run(args)
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        Err("Collection process-group supervision is unsupported on this platform".into())
    }
}

pub(super) fn group_absent(group: u32) -> ToolResult<bool> {
    #[cfg(unix)]
    {
        unix::group_absent(group)
    }
    #[cfg(not(unix))]
    {
        let _ = group;
        Err("Collection process-group inventory is unsupported on this platform".into())
    }
}

#[cfg(unix)]
mod unix {
    use crate::{ToolResult, common, policy::ExitError};
    use clap::Parser;
    #[cfg(target_os = "linux")]
    use nix::{
        errno::Errno,
        sys::{
            prctl::set_child_subreaper,
            wait::{WaitPidFlag, WaitStatus, waitpid},
        },
        unistd::Pid,
    };
    use serde::Serialize;
    use std::{
        fs,
        io::{Read, Seek, SeekFrom},
        os::unix::process::{CommandExt, ExitStatusExt},
        path::PathBuf,
        process::{Child, Command, ExitStatus, Stdio},
        sync::{
            Arc, LazyLock,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const POLL: Duration = Duration::from_millis(20);
    const TERM_GRACE: Duration = Duration::from_secs(1);
    const KILL_GRACE: Duration = Duration::from_secs(2);
    const INVENTORY_TIMEOUT: Duration = Duration::from_secs(2);
    const MAX_INVENTORY_BYTES: u64 = 16 * 1024 * 1024;

    #[derive(Parser)]
    #[command(no_binary_name = true)]
    struct Args {
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        timeout_seconds: u64,
        #[arg(long)]
        receipt: PathBuf,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    }

    #[derive(Serialize)]
    struct Receipt<'a> {
        schema_version: u8,
        process_group: Option<u32>,
        cleanup_complete: bool,
        leader_reaped: bool,
        exit_code: Option<u8>,
        timed_out: bool,
        command: &'a [String],
        timeout_seconds: u64,
        started_unix_ms: u128,
        duration_ms: u128,
        errors: Vec<String>,
    }

    impl Receipt<'_> {
        fn write(&mut self, args: &Args, started: Instant) -> ToolResult<()> {
            self.duration_ms = started.elapsed().as_millis();
            let mut value = serde_json::to_value(&*self)?;
            value["spawned"] = self.process_group.is_some().into();
            common::write_json(&args.receipt, &value)
        }
    }

    struct OwnedChild {
        child: Child,
        group: u32,
        status: Option<ExitStatus>,
    }

    impl OwnedChild {
        fn reap(&mut self) -> ToolResult<bool> {
            if self.status.is_none() {
                self.status = self.child.try_wait()?;
            }
            Ok(self.status.is_some())
        }

        #[cfg(target_os = "linux")]
        fn reap_descendants(&self) -> ToolResult<()> {
            // Never steal the leader's status from std::process::Child, or another group's child.
            if self.status.is_none() {
                return Ok(());
            }
            let target = Pid::from_raw(-i32::try_from(self.group)?);
            let started = Instant::now();
            loop {
                if started.elapsed() >= KILL_GRACE {
                    return Err("Owned descendant reaping exceeded its deadline".into());
                }
                match waitpid(target, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => return Ok(()),
                    Ok(WaitStatus::Exited(..) | WaitStatus::Signaled(..)) | Err(Errno::EINTR) => {}
                    Ok(status) => {
                        return Err(
                            format!("Unexpected owned child wait status: {status:?}").into()
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }

        fn stopped(&mut self) -> ToolResult<bool> {
            // Reap first: otherwise our own zombie leader can make the inventory nonempty.
            self.reap()?;
            #[cfg(target_os = "linux")]
            self.reap_descendants()?;
            let absent = group_absent(self.group)?;
            let reaped = self.reap()?;
            #[cfg(target_os = "linux")]
            self.reap_descendants()?;
            Ok(reaped && absent)
        }

        fn wait_stopped(&mut self, grace: Duration, errors: &mut Vec<String>) -> bool {
            let started = Instant::now();
            let mut inventory_error_reported = false;
            loop {
                match self.stopped() {
                    Ok(true) => return true,
                    Ok(false) => {}
                    Err(error) => {
                        if !inventory_error_reported {
                            errors.push(format!("Cannot prove collection stopped: {error}"));
                            inventory_error_reported = true;
                        }
                    }
                }
                if started.elapsed() >= grace {
                    return false;
                }
                thread::sleep(POLL);
            }
        }

        fn cleanup(&mut self, errors: &mut Vec<String>) -> bool {
            match self.stopped() {
                Ok(true) => return true,
                Ok(false) => {}
                Err(error) => errors.push(format!("Initial cleanup inventory failed: {error}")),
            }
            if let Err(error) = signal_group(self.group, "TERM") {
                errors.push(format!("Collection TERM failed: {error}"));
            }
            if self.wait_stopped(TERM_GRACE, errors) {
                return true;
            }
            if let Err(error) = signal_group(self.group, "KILL") {
                errors.push(format!("Collection KILL failed: {error}"));
            }
            let stopped = self.wait_stopped(KILL_GRACE, errors);
            if !stopped {
                errors.push(
                    "Collection cleanup incomplete: the leader is unreaped or its process group still exists (including zombies)".to_owned(),
                );
            }
            stopped
        }
    }

    #[derive(Clone, Copy)]
    enum Completion {
        Leader,
        Timeout,
        Interrupted,
        Failed,
    }

    pub(super) fn run(args: &[String]) -> ToolResult<()> {
        let args = Args::try_parse_from(args)?;
        let interrupted = interruption_flag()?;
        supervise(&args, interrupted)
    }

    fn interruption_flag() -> ToolResult<&'static AtomicBool> {
        static FLAG: LazyLock<Result<Arc<AtomicBool>, String>> = LazyLock::new(|| {
            let interrupted = Arc::new(AtomicBool::new(false));
            let handler_flag = Arc::clone(&interrupted);
            ctrlc::set_handler(move || handler_flag.store(true, Ordering::SeqCst)).map_err(
                |error| format!("Cannot install collection termination handler: {error}"),
            )?;
            Ok(interrupted)
        });
        match &*FLAG {
            Ok(flag) => Ok(flag.as_ref()),
            Err(error) => Err(error.clone().into()),
        }
    }

    #[cfg(target_os = "linux")]
    fn initialize_subreaper() -> ToolResult<()> {
        // Process-wide and one-way: restoring a prior flag races concurrent owned collections.
        static SETUP: LazyLock<Result<(), String>> = LazyLock::new(|| {
            set_child_subreaper(true)
                .map_err(|error| format!("Cannot establish collection child subreaper: {error}"))
        });
        match &*SETUP {
            Ok(()) => Ok(()),
            Err(error) => Err(error.clone().into()),
        }
    }

    fn initial_receipt(args: &Args) -> ToolResult<Receipt<'_>> {
        // A lifecycle receipt belongs to exactly one invocation.
        match fs::symlink_metadata(&args.receipt) {
            Ok(_) => return Err("Collection lifecycle receipt already exists".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(Receipt {
            schema_version: 1,
            process_group: None,
            cleanup_complete: false,
            leader_reaped: false,
            exit_code: None,
            timed_out: false,
            command: &args.command,
            timeout_seconds: args.timeout_seconds,
            started_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
            duration_ms: 0,
            errors: Vec::new(),
        })
    }

    fn supervise(args: &Args, interrupted: &AtomicBool) -> ToolResult<()> {
        let started = Instant::now();
        let mut receipt = initial_receipt(args)?;
        receipt.write(args, started)?;
        #[cfg(target_os = "linux")]
        if let Err(error) = initialize_subreaper() {
            receipt.cleanup_complete = true;
            receipt.exit_code = Some(1);
            receipt.errors.push(error.to_string());
            receipt.write(args, started)?;
            return Err(ExitError::new(1, receipt.errors.join("; ")).into());
        }
        let Some((program, arguments)) = args.command.split_first() else {
            return Err("Collection command is required".into());
        };
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(args.timeout_seconds))
            .ok_or("Collection deadline is outside the monotonic clock range")?;
        let child = Command::new(program)
            .args(arguments)
            .process_group(0)
            .spawn();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                receipt.cleanup_complete = true;
                receipt.exit_code = Some(127);
                receipt
                    .errors
                    .push(format!("Collection spawn failed: {error}"));
                if let Err(error) = receipt.write(args, started) {
                    receipt
                        .errors
                        .push(format!("Cannot publish spawn-failure receipt: {error}"));
                }
                return Err(ExitError::new(127, receipt.errors.join("; ")).into());
            }
        };
        let mut owned = OwnedChild {
            group: child.id(),
            child,
            status: None,
        };
        receipt.process_group = Some(owned.group);
        let completion = match receipt.write(args, started) {
            Ok(()) => monitor(&mut owned, deadline, interrupted, &mut receipt.errors),
            Err(error) => {
                receipt
                    .errors
                    .push(format!("Cannot checkpoint spawned collection: {error}"));
                Completion::Failed
            }
        };
        finish(args, started, owned, completion, receipt)
    }

    fn monitor(
        owned: &mut OwnedChild,
        deadline: Instant,
        interrupted: &AtomicBool,
        errors: &mut Vec<String>,
    ) -> Completion {
        loop {
            if interrupted.load(Ordering::SeqCst) {
                errors
                    .push("Collection interrupted; terminating its owned process group".to_owned());
                return Completion::Interrupted;
            }
            if Instant::now() >= deadline {
                errors.push("Collection measurement deadline expired".to_owned());
                return Completion::Timeout;
            }
            match owned.reap() {
                Ok(true) => return Completion::Leader,
                Ok(false) => {}
                Err(error) => {
                    errors.push(format!("Cannot reap collection leader: {error}"));
                    return Completion::Failed;
                }
            }
            thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        }
    }

    fn finish(
        args: &Args,
        started: Instant,
        mut owned: OwnedChild,
        completion: Completion,
        mut receipt: Receipt<'_>,
    ) -> ToolResult<()> {
        let mut exit_code = match completion {
            Completion::Timeout => {
                receipt.timed_out = true;
                124
            }
            Completion::Interrupted => 130,
            Completion::Failed => 1,
            Completion::Leader => owned.status.map_or(1, status_code),
        };
        let already_stopped = match owned.stopped() {
            Ok(stopped) => stopped,
            Err(error) => {
                receipt
                    .errors
                    .push(format!("Post-leader inventory failed: {error}"));
                false
            }
        };
        if matches!(completion, Completion::Failed)
            && let Some(status) = owned.status
            && !status.success()
        {
            exit_code = status_code(status);
        }
        if matches!(completion, Completion::Leader) && !already_stopped {
            receipt.errors.push(
                "Collection leader exited without an absent process group; descendant cleanup was required".to_owned(),
            );
            if exit_code == 0 {
                exit_code = 1;
            }
        }
        receipt.cleanup_complete = already_stopped || owned.cleanup(&mut receipt.errors);
        receipt.leader_reaped = owned.status.is_some();
        if !receipt.cleanup_complete && exit_code == 0 {
            exit_code = 1;
        }
        receipt.exit_code = Some(exit_code);
        if let Err(error) = receipt.write(args, started) {
            receipt
                .errors
                .push(format!("Cannot publish final collection receipt: {error}"));
            if exit_code == 0 {
                exit_code = 1;
            }
        }
        if exit_code == 0 {
            Ok(())
        } else {
            Err(ExitError::new(
                exit_code,
                format!(
                    "Collection exited with status {exit_code}: {}",
                    receipt.errors.join("; ")
                ),
            )
            .into())
        }
    }

    fn status_code(status: ExitStatus) -> u8 {
        status.code().map_or_else(
            || u8::try_from(128 + status.signal().unwrap_or(0)).unwrap_or(1),
            |code| u8::try_from(code).unwrap_or(1),
        )
    }

    fn validate_group(group: u32) -> ToolResult<()> {
        if group == 0 || group > i32::MAX.cast_unsigned() {
            return Err(
                "Collection process group must be a positive POSIX process-group ID".into(),
            );
        }
        Ok(())
    }

    enum GroupState {
        Absent,
        Zombies,
        Live,
    }

    pub(super) fn group_absent(group: u32) -> ToolResult<bool> {
        validate_group(group)?;
        Ok(matches!(group_state(group)?, GroupState::Absent))
    }

    fn group_state(group: u32) -> ToolResult<GroupState> {
        let output = bounded_output(
            Command::new("/bin/ps").args(["-axo", "pgid=,stat="]),
            INVENTORY_TIMEOUT,
        )?;
        let text = std::str::from_utf8(&output)?;
        parse_group_inventory(text, group)
    }

    fn parse_group_inventory(text: &str, group: u32) -> ToolResult<GroupState> {
        let mut present = false;
        let mut live = false;
        let mut rows = 0_usize;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let mut columns = line.split_whitespace();
            let pgid: u32 = columns
                .next()
                .ok_or("Missing inventory process group")?
                .parse()?;
            let state = columns.next().ok_or("Missing inventory process state")?;
            if columns.next().is_some() {
                return Err("Malformed process inventory".into());
            }
            rows += 1;
            if pgid == group {
                present = true;
                // Unknown kernel states are conservatively live. Their spelling
                // cannot hide group membership or establish absence.
                live |= !state.starts_with('Z');
            }
        }
        if rows == 0 {
            return Err("Process inventory was empty; absence cannot be established".into());
        }
        Ok(if live {
            GroupState::Live
        } else if present {
            GroupState::Zombies
        } else {
            GroupState::Absent
        })
    }

    fn signal_group(group: u32, signal: &str) -> ToolResult<()> {
        validate_group(group)?;
        bounded_output(
            Command::new("/bin/kill")
                .arg(format!("-{signal}"))
                .arg("--")
                .arg(format!("-{group}")),
            Duration::from_secs(1),
        )?;
        Ok(())
    }

    fn bounded_output(command: &mut Command, timeout: Duration) -> ToolResult<Vec<u8>> {
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        let mut child = command
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?)
            .spawn()?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if started.elapsed() >= timeout {
                child.kill()?;
                let reap_started = Instant::now();
                while child.try_wait()?.is_none() {
                    if reap_started.elapsed() >= KILL_GRACE {
                        return Err("Process inventory/signal utility could not be reaped".into());
                    }
                    thread::sleep(POLL);
                }
                return Err("Process inventory/signal utility exceeded its deadline".into());
            }
            thread::sleep(POLL);
        };
        let output = read_bounded(&mut stdout)?;
        let diagnostics = read_bounded(&mut stderr)?;
        if !status.success() || !diagnostics.is_empty() {
            return Err(format!(
                "Process inventory/signal utility failed ({status}): {}",
                String::from_utf8_lossy(&diagnostics).trim()
            )
            .into());
        }
        Ok(output)
    }

    fn read_bounded(file: &mut fs::File) -> ToolResult<Vec<u8>> {
        if file.metadata()?.len() > MAX_INVENTORY_BYTES {
            return Err("Process inventory/signal utility output exceeded its size bound".into());
        }
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(MAX_INVENTORY_BYTES + 1).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len())? > MAX_INVENTORY_BYTES {
            return Err("Process inventory/signal utility output exceeded its size bound".into());
        }
        Ok(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::{
            Args, GroupState, group_absent, group_state, parse_group_inventory, supervise,
        };
        use crate::{ToolResult, policy::ExitError};
        use serde_json::Value;
        use std::{
            fs,
            os::unix::process::CommandExt,
            path::Path,
            process::Command,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
            thread,
            time::{Duration, Instant},
        };

        struct FixtureCleanup<'a>(&'a Args);

        impl Drop for FixtureCleanup<'_> {
            fn drop(&mut self) {
                let Ok(value) = receipt(self.0) else {
                    return;
                };
                let Ok(group) = recorded_group(&value) else {
                    return;
                };
                let Ok(group) = i32::try_from(group) else {
                    return;
                };
                if group <= 1 {
                    return;
                }
                // Independent fallback: a failed supervisor must not leak test writers.
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", "--"])
                    .arg(format!("-{group}"))
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                #[cfg(target_os = "linux")]
                {
                    let started = Instant::now();
                    while started.elapsed() < Duration::from_secs(2) {
                        match super::waitpid(
                            super::Pid::from_raw(-group),
                            Some(super::WaitPidFlag::WNOHANG),
                        ) {
                            Ok(super::WaitStatus::StillAlive) => {
                                thread::sleep(Duration::from_millis(20));
                            }
                            Ok(super::WaitStatus::Exited(..) | super::WaitStatus::Signaled(..))
                            | Err(super::Errno::EINTR) => {}
                            _ => break,
                        }
                    }
                }
            }
        }

        #[test]
        fn uncertain_kernel_states_cannot_hide_owned_processes() -> ToolResult<()> {
            let inventory = "21 ?E\n42 Z\n";
            assert!(!matches!(
                parse_group_inventory(inventory, 21)?,
                GroupState::Absent
            ));
            assert!(matches!(
                parse_group_inventory(inventory, 43)?,
                GroupState::Absent
            ));
            assert!(parse_group_inventory("invalid ?E\n", 21).is_err());
            Ok(())
        }

        fn args(directory: &Path, timeout_seconds: u64, script: &str, extra: &[String]) -> Args {
            let mut command = vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                script.to_owned(),
                "collection-fixture".to_owned(),
            ];
            command.extend_from_slice(extra);
            Args {
                timeout_seconds,
                receipt: directory.join("collection-lifecycle.json"),
                command,
            }
        }

        fn receipt(args: &Args) -> ToolResult<Value> {
            Ok(serde_json::from_slice(&fs::read(&args.receipt)?)?)
        }

        fn exit_status(result: ToolResult<()>) -> ToolResult<u8> {
            let error = result.err().ok_or("Expected failed collection")?;
            Ok(error
                .downcast_ref::<ExitError>()
                .ok_or("Expected preserved ExitError")?
                .status)
        }

        fn recorded_group(value: &Value) -> ToolResult<u32> {
            Ok(u32::try_from(
                value["process_group"]
                    .as_u64()
                    .ok_or("Missing process group")?,
            )?)
        }

        #[test]
        fn successful_command_preserves_literal_arguments_and_environment() -> ToolResult<()> {
            let directory = tempfile::tempdir()?;
            let output = directory.path().join("arguments");
            let literal = "literal space ; $(not-a-command) *".to_owned();
            let args = args(
                directory.path(),
                5,
                "printf '%s\\n%s\\n' \"$1\" \"$PATH\" > \"$2\"",
                &[literal.clone(), output.to_string_lossy().into_owned()],
            );
            supervise(&args, &AtomicBool::new(false))?;
            let expected = format!("{literal}\n{}\n", std::env::var("PATH")?);
            assert_eq!(fs::read_to_string(output)?, expected);
            let value = receipt(&args)?;
            assert_eq!(value["exit_code"], 0);
            assert_eq!(value["cleanup_complete"], true);
            assert_eq!(value["leader_reaped"], true);
            assert_eq!(value["command"], serde_json::to_value(&args.command)?);
            assert!(group_absent(recorded_group(&value)?)?);
            Ok(())
        }

        #[test]
        fn child_nonzero_status_is_not_replaced_by_supervisor_status() -> ToolResult<()> {
            let directory = tempfile::tempdir()?;
            let args = args(directory.path(), 5, "exit 37", &[]);
            assert_eq!(exit_status(supervise(&args, &AtomicBool::new(false)))?, 37);
            let value = receipt(&args)?;
            assert_eq!(value["exit_code"], 37);
            assert_eq!(value["timed_out"], false);
            assert_eq!(value["cleanup_complete"], true);
            assert!(group_absent(recorded_group(&value)?)?);
            Ok(())
        }

        #[test]
        fn signal_exit_is_preserved() -> ToolResult<()> {
            let directory = tempfile::tempdir()?;
            let args = args(directory.path(), 5, "kill -TERM $$", &[]);
            assert_eq!(exit_status(supervise(&args, &AtomicBool::new(false)))?, 143);
            assert_eq!(receipt(&args)?["cleanup_complete"], true);
            Ok(())
        }

        #[test]
        fn timeout_escalates_past_ignored_term_and_reaps_leader() -> ToolResult<()> {
            let directory = tempfile::tempdir()?;
            let args = args(
                directory.path(),
                1,
                "trap '' TERM; while :; do :; done",
                &[],
            );
            let _cleanup = FixtureCleanup(&args);
            let started = Instant::now();
            assert_eq!(exit_status(supervise(&args, &AtomicBool::new(false)))?, 124);
            assert!(started.elapsed() < Duration::from_secs(12));
            let value = receipt(&args)?;
            assert_eq!(value["timed_out"], true);
            assert_eq!(value["leader_reaped"], true);
            assert_eq!(value["cleanup_complete"], true);
            assert!(group_absent(recorded_group(&value)?)?);
            Ok(())
        }

        #[test]
        fn leader_exit_with_term_ignoring_writer_cannot_pass() -> ToolResult<()> {
            let directory = tempfile::tempdir()?;
            let writer = directory.path().join("writer");
            let ready = directory.path().join("ready");
            let args = args(
                directory.path(),
                5,
                "(trap '' TERM; printf ready > \"$2\"; while :; do printf x >> \"$1\"; sleep 0.05; done) & while [ ! -f \"$2\" ]; do sleep 0.01; done; exit 0",
                &[
                    writer.to_string_lossy().into_owned(),
                    ready.to_string_lossy().into_owned(),
                ],
            );
            let _cleanup = FixtureCleanup(&args);
            assert_eq!(exit_status(supervise(&args, &AtomicBool::new(false)))?, 1);
            let value = receipt(&args)?;
            assert_eq!(value["leader_reaped"], true);
            assert_eq!(value["timed_out"], false);
            let group = recorded_group(&value)?;
            let state = group_state(group)?;
            // A host that cannot reap orphan zombies must fail closed, not call them absent.
            assert!(!matches!(state, GroupState::Live));
            assert_eq!(
                value["cleanup_complete"],
                matches!(state, GroupState::Absent)
            );
            #[cfg(target_os = "linux")]
            assert!(matches!(state, GroupState::Absent));
            let finished = fs::read(&writer)?;
            assert!(!finished.is_empty());
            thread::sleep(Duration::from_millis(150));
            assert_eq!(fs::read(writer)?, finished);
            Ok(())
        }

        #[test]
        fn spawn_failure_is_truthful_and_does_not_invent_a_group() -> ToolResult<()> {
            let directory = tempfile::tempdir()?;
            let mut args = args(directory.path(), 5, "exit 0", &[]);
            args.command = vec![
                directory
                    .path()
                    .join("nonexistent-program")
                    .to_string_lossy()
                    .into_owned(),
            ];
            assert_eq!(exit_status(supervise(&args, &AtomicBool::new(false)))?, 127);
            let value = receipt(&args)?;
            assert_eq!(value["spawned"], false);
            assert!(value["process_group"].is_null());
            assert_eq!(value["cleanup_complete"], true);
            assert_eq!(value["leader_reaped"], false);
            Ok(())
        }

        #[test]
        fn pending_receipt_blocks_publication_until_interrupted_group_is_reaped() -> ToolResult<()>
        {
            let directory = tempfile::tempdir()?;
            let args = args(
                directory.path(),
                10,
                "trap '' TERM; while :; do :; done",
                &[],
            );
            let _cleanup = FixtureCleanup(&args);
            let interrupted = Arc::new(AtomicBool::new(false));
            thread::scope(|scope| -> ToolResult<()> {
                let worker = scope.spawn(|| supervise(&args, &interrupted));
                let started = Instant::now();
                let mut observed_pending = false;
                while started.elapsed() < Duration::from_secs(5) {
                    if let Ok(bytes) = fs::read(&args.receipt)
                        && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                        && value["spawned"] == true
                    {
                        observed_pending = value["cleanup_complete"] == false
                            && value["exit_code"].is_null()
                            && value["leader_reaped"] == false;
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                interrupted.store(true, Ordering::SeqCst);
                let result = worker.join().map_err(|_| "Supervisor worker panicked")?;
                assert!(observed_pending);
                assert_eq!(exit_status(result)?, 130);
                Ok(())
            })?;
            let value = receipt(&args)?;
            assert_eq!(value["cleanup_complete"], true);
            assert_eq!(value["leader_reaped"], true);
            assert_eq!(value["timed_out"], false);
            assert!(group_absent(recorded_group(&value)?)?);
            Ok(())
        }

        #[test]
        fn an_unreaped_zombie_group_is_not_absent() -> ToolResult<()> {
            let mut child = Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .process_group(0)
                .spawn()?;
            let group = child.id();
            let observation = (|| -> ToolResult<()> {
                let started = Instant::now();
                loop {
                    if matches!(group_state(group)?, GroupState::Zombies) {
                        assert!(!group_absent(group)?);
                        return Ok(());
                    }
                    if started.elapsed() >= Duration::from_secs(3) {
                        return Err("Fixture never reached its unreaped zombie state".into());
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            })();
            // Reap the fixture even when inventory observation itself failed.
            let status = child.wait()?;
            observation?;
            assert!(status.success());
            assert!(group_absent(group)?);
            Ok(())
        }

        #[test]
        fn cleanup_cannot_steal_an_unrelated_child_exit_status() -> ToolResult<()> {
            let directory = tempfile::tempdir()?;
            let args = args(
                directory.path(),
                1,
                "trap '' TERM; while :; do :; done",
                &[],
            );
            let _cleanup = FixtureCleanup(&args);
            let mut unrelated = Command::new("/bin/sh")
                .args(["-c", "exit 53"])
                .process_group(0)
                .spawn()?;
            let result = supervise(&args, &AtomicBool::new(false));
            let unrelated_status = unrelated.wait()?;
            assert_eq!(unrelated_status.code(), Some(53));
            assert_eq!(exit_status(result)?, 124);
            assert_eq!(receipt(&args)?["cleanup_complete"], true);
            Ok(())
        }

        #[test]
        fn existing_receipt_cannot_be_reused_or_overwritten() -> ToolResult<()> {
            let directory = tempfile::tempdir()?;
            let args = args(directory.path(), 5, "exit 0", &[]);
            fs::write(&args.receipt, b"existing receipt")?;
            assert!(supervise(&args, &AtomicBool::new(false)).is_err());
            assert_eq!(fs::read(args.receipt)?, b"existing receipt");
            assert!(group_absent(0).is_err());
            assert!(group_absent(u32::MAX).is_err());
            Ok(())
        }
    }
}
