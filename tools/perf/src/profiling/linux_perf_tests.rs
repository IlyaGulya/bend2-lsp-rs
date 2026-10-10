use super::{OwnedCommand, ScenarioSession, Session, ToolResult};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};

// Original stderr from hosted ARM run 38075639135, artifact 11678858007.
// This fixture reproduces collector startup failure, never sample evidence.
const HOSTED_STDERR: &str = "Error:\ncpu-clock:u: PMU Hardware doesn't support sampling/overflow-interrupts. Try 'perf stat'\n";

#[test]
fn early_exit_preserves_stderr_and_reaps_collector() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let collector = directory.path().join("perf");
    fs::write(
        &collector,
        r#"#!/bin/sh
printf '%s\n' 'Error:' "cpu-clock:u: PMU Hardware doesn't support sampling/overflow-interrupts. Try 'perf stat'" >&2
exit 255
"#,
    )?;
    fs::set_permissions(&collector, fs::Permissions::from_mode(0o700))?;
    // Isolate PATH in a subprocess; never race other tests' native commands.
    let mut child = OwnedCommand(
        Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "profiling::sessions::linux_perf_tests::startup_failure_fixture",
                "--nocapture",
            ])
            .env("BEND_PERF_STARTUP_FAILURE_DIR", directory.path())
            .env("PATH", directory.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let deadline = super::Instant::now() + super::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        assert!(
            super::Instant::now() < deadline,
            "perf startup failure fixture did not finish promptly"
        );
        super::thread::sleep(super::Duration::from_millis(10));
    };
    let mut evidence = String::new();
    if let Some(mut stdout) = child.0.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut evidence)?;
    }
    if let Some(mut stderr) = child.0.stderr.take() {
        std::io::Read::read_to_string(&mut stderr, &mut evidence)?;
    }
    assert!(status.success(), "Startup failure regression: {evidence}");
    Ok(())
}

#[test]
fn startup_failure_fixture() -> ToolResult<()> {
    let Some(directory) = std::env::var_os("BEND_PERF_STARTUP_FAILURE_DIR") else {
        return Ok(());
    };
    let directory = std::path::PathBuf::from(directory);
    let mut session = Session::new("perf", "cpu", &directory);
    let error = session
        .started(123)
        .expect_err("Failed collector became ready");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("perf exited before enable acknowledgement"));
    assert!(diagnostic.contains("profiler.stderr.log"));
    assert!(
        diagnostic.contains(HOSTED_STDERR),
        "Missing raw stderr: {diagnostic}"
    );
    assert!(
        !session
            .phases
            .iter()
            .any(|phase| phase["name"] == "profiler.ready")
    );
    let cleanup = session
        .abort()
        .expect_err("Failed collector exit was discarded");
    assert!(cleanup.to_string().contains("Profiler finalization failed"));
    assert!(session.child.is_none(), "Collector was not reaped");
    assert!(session.control.is_none(), "Control FIFO remains owned");
    for name in ["perf-control.fifo", "perf-ack.fifo"] {
        assert!(!directory.join(name).exists(), "Leaked FIFO {name}");
    }
    assert_eq!(
        fs::read(directory.join("profiler.stderr.log"))?,
        HOSTED_STDERR.as_bytes()
    );
    Ok(())
}
