//! Bounded, single-process OS memory sampling for production diagnostics.
//!
//! Only the caller-supplied process is observed: the LSP server itself or a
//! compiler child that this server spawned and therefore owns. There is no
//! system-wide process enumeration, remote access or heap instrumentation.
//! Every function blocks on ordinary OS reads and is intended to be called
//! from `spawn_blocking`, never from the LSP async hot path.

use serde::Serialize;
use std::{error::Error, fmt};
#[cfg(target_os = "linux")]
use nix::fcntl::{OFlag, open, openat};
#[cfg(target_os = "linux")]
use nix::sys::stat::Mode;
#[cfg(target_os = "linux")]
use std::io::Read;

pub(crate) const MEMORY_SCOPE: &str = "single process only: the LSP server or one compiler child owned by it; excludes descendant processes, unrelated processes, system memory, heap-live bytes and allocator overhead; OS figures are kernel-approximate";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessMemoryError {
    InvalidPid,
    ProcessExited,
    IdentityChanged,
    SamplingFailed,
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    )))]
    PlatformUnsupported,
}

impl fmt::Display for ProcessMemoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPid => formatter.write_str("process memory requires a nonzero PID"),
            Self::ProcessExited => formatter.write_str("process memory target is no longer running"),
            Self::IdentityChanged => {
                formatter.write_str("process identity changed; refusing a reused PID")
            }
            Self::SamplingFailed => {
                formatter.write_str("operating system process-memory query failed")
            }
            #[cfg(not(any(
                target_os = "linux",
                target_os = "macos",
                target_os = "windows"
            )))]
            Self::PlatformUnsupported => {
                formatter.write_str("process memory is unsupported on this operating system")
            }
        }
    }
}

impl Error for ProcessMemoryError {}

/// Serializable description of one reported memory field. Definitions travel
/// with every sample so unlike resident, private-commit, peak and physical
/// footprint values are never mistaken for interchangeable measurements.
#[derive(Clone, Copy, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryMetricDefinition {
    pub(crate) metric: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) source: &'static str,
    pub(crate) definition: &'static str,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MemoryMetricUnavailable {
    pub(crate) metric: &'static str,
    pub(crate) reason: &'static str,
}

#[derive(Clone, Debug, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProcessMemorySample {
    pub(crate) pid: u32,
    pub(crate) scope: &'static str,
    pub(crate) resident_bytes: Option<u64>,
    pub(crate) peak_resident_bytes: Option<u64>,
    pub(crate) private_bytes: Option<u64>,
    pub(crate) physical_footprint_bytes: Option<u64>,
    pub(crate) unavailable: Vec<MemoryMetricUnavailable>,
    pub(crate) definitions: &'static [MemoryMetricDefinition],
}

/// Identity captured before the first sample. On every later sample the OS
/// start marker (or the retained Windows process object) must still match, so
/// a recycled PID can never be attributed to a compiler child we own.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProcessIdentity {
    pid: u32,
    start_marker: u64,
}

impl ProcessIdentity {
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    ))]
    fn verify(self, observed_pid: u32, observed_start_marker: u64) -> Result<(), ProcessMemoryError> {
        if observed_pid != self.pid || observed_start_marker != self.start_marker {
            return Err(ProcessMemoryError::IdentityChanged);
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
const METRIC_DEFINITIONS: &[MemoryMetricDefinition] = &[
    MemoryMetricDefinition {
        metric: "residentBytes",
        unit: "bytes",
        source: "/proc/PID/status VmRSS",
        definition: "current resident set (RssAnon + RssFile + RssShmem); kernel approximate accounting; not heap-live bytes or private commit",
    },
    MemoryMetricDefinition {
        metric: "peakResidentBytes",
        unit: "bytes",
        source: "/proc/PID/status VmHWM",
        definition: "process-lifetime peak resident set; not a phase delta",
    },
    MemoryMetricDefinition {
        metric: "privateBytes",
        unit: "bytes",
        source: "unavailable",
        definition: "Linux procfs exposes no private-commit counter; RssAnon is resident anonymous memory and is not equivalent",
    },
    MemoryMetricDefinition {
        metric: "physicalFootprintBytes",
        unit: "bytes",
        source: "unavailable",
        definition: "Darwin physical footprint is not available on Linux",
    },
];

#[cfg(target_os = "macos")]
const METRIC_DEFINITIONS: &[MemoryMetricDefinition] = &[
    MemoryMetricDefinition {
        metric: "residentBytes",
        unit: "bytes",
        source: "libproc proc_pid_rusage RUSAGE_INFO_V2 ri_resident_size",
        definition: "current resident memory; not physical footprint or heap-live bytes",
    },
    MemoryMetricDefinition {
        metric: "peakResidentBytes",
        unit: "bytes",
        source: "unavailable",
        definition: "RUSAGE_INFO_V2 exposes no resident high watermark; physical footprint is never substituted",
    },
    MemoryMetricDefinition {
        metric: "privateBytes",
        unit: "bytes",
        source: "unavailable",
        definition: "RUSAGE_INFO_V2 exposes no private-commit counter; ri_phys_footprint is a separate ledger",
    },
    MemoryMetricDefinition {
        metric: "physicalFootprintBytes",
        unit: "bytes",
        source: "libproc proc_pid_rusage RUSAGE_INFO_V2 ri_phys_footprint",
        definition: "Darwin physical-footprint ledger charge, distinct from resident size and including compressed-memory accounting",
    },
];

#[cfg(target_os = "windows")]
const METRIC_DEFINITIONS: &[MemoryMetricDefinition] = &[
    MemoryMetricDefinition {
        metric: "residentBytes",
        unit: "bytes",
        source: "PSAPI GetProcessMemoryInfo PROCESS_MEMORY_COUNTERS_EX WorkingSetSize",
        definition: "current resident working set including shared pages; not private commit",
    },
    MemoryMetricDefinition {
        metric: "peakResidentBytes",
        unit: "bytes",
        source: "PSAPI PROCESS_MEMORY_COUNTERS_EX PeakWorkingSetSize",
        definition: "process-lifetime peak resident working set; not a phase delta",
    },
    MemoryMetricDefinition {
        metric: "privateBytes",
        unit: "bytes",
        source: "PSAPI PROCESS_MEMORY_COUNTERS_EX PrivateUsage",
        definition: "private committed memory (commit charge); not resident memory or actual pagefile occupancy",
    },
    MemoryMetricDefinition {
        metric: "physicalFootprintBytes",
        unit: "bytes",
        source: "unavailable",
        definition: "Darwin physical footprint is not available on Windows",
    },
];

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
)))]
const METRIC_DEFINITIONS: &[MemoryMetricDefinition] = &[];

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
))]
struct RawMemory {
    resident_bytes: u64,
    peak_resident_bytes: Option<u64>,
    private_bytes: Option<u64>,
    physical_footprint_bytes: Option<u64>,
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
))]
fn finish_sample(identity: ProcessIdentity, raw: RawMemory) -> ProcessMemorySample {
    ProcessMemorySample {
        pid: identity.pid,
        scope: MEMORY_SCOPE,
        resident_bytes: Some(raw.resident_bytes),
        peak_resident_bytes: raw.peak_resident_bytes,
        private_bytes: raw.private_bytes,
        physical_footprint_bytes: raw.physical_footprint_bytes,
        unavailable: build_unavailable(&raw),
        definitions: METRIC_DEFINITIONS,
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
))]
fn build_unavailable(raw: &RawMemory) -> Vec<MemoryMetricUnavailable> {
    let mut unavailable = Vec::new();
    if raw.peak_resident_bytes.is_none() {
        unavailable.push(MemoryMetricUnavailable {
            metric: "peakResidentBytes",
            reason: peak_resident_unavailable(),
        });
    }
    if raw.private_bytes.is_none() {
        unavailable.push(MemoryMetricUnavailable {
            metric: "privateBytes",
            reason: private_bytes_unavailable(),
        });
    }
    if raw.physical_footprint_bytes.is_none() {
        unavailable.push(MemoryMetricUnavailable {
            metric: "physicalFootprintBytes",
            reason: physical_footprint_unavailable(),
        });
    }
    unavailable
}

#[cfg(target_os = "linux")]
fn peak_resident_unavailable() -> &'static str {
    "VmHWM is absent from procfs status; the resident high watermark is unavailable"
}

#[cfg(target_os = "macos")]
fn peak_resident_unavailable() -> &'static str {
    "RUSAGE_INFO_V2 exposes no resident high watermark; physical footprint is never substituted"
}

#[cfg(target_os = "windows")]
fn peak_resident_unavailable() -> &'static str {
    "the platform process-memory backend did not report a peak resident working set"
}

#[cfg(target_os = "linux")]
fn private_bytes_unavailable() -> &'static str {
    "Linux procfs exposes no private-commit counter; RssAnon is resident anonymous memory and is not equivalent"
}

#[cfg(target_os = "macos")]
fn private_bytes_unavailable() -> &'static str {
    "RUSAGE_INFO_V2 exposes no private-commit counter; ri_phys_footprint is a separate ledger"
}

#[cfg(target_os = "windows")]
fn private_bytes_unavailable() -> &'static str {
    "the platform process-memory backend did not report private commit"
}

#[cfg(target_os = "linux")]
fn physical_footprint_unavailable() -> &'static str {
    "Darwin physical footprint is not available on Linux"
}

#[cfg(target_os = "macos")]
fn physical_footprint_unavailable() -> &'static str {
    "the platform process-memory backend did not report physical footprint"
}

#[cfg(target_os = "windows")]
fn physical_footprint_unavailable() -> &'static str {
    "Darwin physical footprint is not available on Windows"
}

/// Captures process identity and retains whatever OS object is needed to keep
/// sampling the same process. Construct it immediately after spawning a child
/// this server owns, or with `std::process::id()` for the server itself.
pub(crate) struct ProcessMemoryObserver {
    identity: ProcessIdentity,
    #[cfg(target_os = "linux")]
    directory: std::fs::File,
    #[cfg(target_os = "windows")]
    process: winsafe::guard::CloseHandleGuard<winsafe::HPROCESS>,
}

#[cfg(target_os = "linux")]
impl ProcessMemoryObserver {
    pub(crate) fn capture(pid: u32) -> Result<Self, ProcessMemoryError> {
        Self::capture_linux_at(std::path::Path::new("/proc"), pid)
    }

    fn capture_linux_at(root: &std::path::Path, pid: u32) -> Result<Self, ProcessMemoryError> {
        if pid == 0 {
            return Err(ProcessMemoryError::InvalidPid);
        }
        // This directory descriptor stays bound to the original process even if
        // /proc/PID is reused. All later reads are relative to this descriptor.
        let path = root.join(pid.to_string());
        let directory = std::fs::File::from(
            open(
                path.as_path(),
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| io_failure(&std::io::Error::from(error)))?,
        );
        let stat = linux_read_proc_file(&directory, "stat")
            .map_err(|error| io_failure(&error))?;
        let (observed_pid, start_marker) = linux_live_identity(&stat)?;
        if observed_pid != pid {
            return Err(ProcessMemoryError::IdentityChanged);
        }
        Ok(Self {
            identity: ProcessIdentity { pid, start_marker },
            directory,
        })
    }

    pub(crate) fn sample(&self) -> Result<ProcessMemorySample, ProcessMemoryError> {
        linux_sample_with(self.identity, |name| linux_read_proc_file(&self.directory, name))
    }
}

#[cfg(target_os = "linux")]
fn linux_read_proc_file(directory: &std::fs::File, name: &str) -> std::io::Result<String> {
    let mut file = std::fs::File::from(openat(
        directory,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?);
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(target_os = "linux")]
fn linux_sample_with(
    identity: ProcessIdentity,
    mut read: impl FnMut(&str) -> std::io::Result<String>,
) -> Result<ProcessMemorySample, ProcessMemoryError> {
    let stat = read("stat").map_err(|error| io_failure(&error))?;
    let (observed_pid, start_marker) = linux_live_identity(&stat)?;
    identity.verify(observed_pid, start_marker)?;

    let text = read("status").map_err(|error| io_failure(&error))?;
    // A process can exit or be reaped between the identity and memory reads.
    // Refuse the metrics if the original process changed state or disappeared.
    let stat = read("stat").map_err(|error| io_failure(&error))?;
    let (observed_pid, start_marker) = linux_live_identity(&stat)?;
    identity.verify(observed_pid, start_marker)?;
    let status = linux_memory_status(&text)?;
    Ok(finish_sample(
        identity,
        RawMemory {
            resident_bytes: status.resident_bytes,
            peak_resident_bytes: status.peak_resident_bytes,
            private_bytes: None,
            physical_footprint_bytes: None,
        },
    ))
}

#[cfg(target_os = "macos")]
impl ProcessMemoryObserver {
    pub(crate) fn capture(pid: u32) -> Result<Self, ProcessMemoryError> {
        if pid == 0 {
            return Err(ProcessMemoryError::InvalidPid);
        }
        let info = macos_usage(pid)?;
        if info.ri_proc_exit_abstime != 0 {
            return Err(ProcessMemoryError::ProcessExited);
        }
        if info.ri_proc_start_abstime == 0 {
            return Err(ProcessMemoryError::SamplingFailed);
        }
        Ok(Self {
            identity: ProcessIdentity {
                pid,
                start_marker: info.ri_proc_start_abstime,
            },
        })
    }

    pub(crate) fn sample(&self) -> Result<ProcessMemorySample, ProcessMemoryError> {
        let info = macos_usage(self.identity.pid)?;
        if info.ri_proc_exit_abstime != 0 {
            return Err(ProcessMemoryError::ProcessExited);
        }
        self.identity
            .verify(self.identity.pid, info.ri_proc_start_abstime)?;
        Ok(finish_sample(
            self.identity,
            RawMemory {
                resident_bytes: info.ri_resident_size,
                peak_resident_bytes: None,
                private_bytes: None,
                physical_footprint_bytes: Some(info.ri_phys_footprint),
            },
        ))
    }
}

#[cfg(target_os = "windows")]
impl ProcessMemoryObserver {
    pub(crate) fn capture(pid: u32) -> Result<Self, ProcessMemoryError> {
        if pid == 0 {
            return Err(ProcessMemoryError::InvalidPid);
        }
        let process = winsafe::HPROCESS::OpenProcess(
            winsafe::co::PROCESS::QUERY_LIMITED_INFORMATION | winsafe::co::PROCESS::SYNCHRONIZE,
            false,
            pid,
        )
        .map_err(|error| {
            if matches!(error, winsafe::co::ERROR::INVALID_PARAMETER) {
                ProcessMemoryError::ProcessExited
            } else {
                ProcessMemoryError::SamplingFailed
            }
        })?;
        let state = process
            .WaitForSingleObject(Some(0))
            .map_err(|_| ProcessMemoryError::SamplingFailed)?;
        if state != winsafe::co::WAIT::TIMEOUT {
            return Err(ProcessMemoryError::ProcessExited);
        }
        let (creation, _, _, _) = process
            .GetProcessTimes()
            .map_err(|_| ProcessMemoryError::SamplingFailed)?;
        let start_marker = filetime_value(creation);
        if start_marker == 0 {
            return Err(ProcessMemoryError::SamplingFailed);
        }
        Ok(Self {
            identity: ProcessIdentity { pid, start_marker },
            process,
        })
    }

    pub(crate) fn sample(&self) -> Result<ProcessMemorySample, ProcessMemoryError> {
        let state = self
            .process
            .WaitForSingleObject(Some(0))
            .map_err(|_| ProcessMemoryError::SamplingFailed)?;
        if state != winsafe::co::WAIT::TIMEOUT {
            return Err(ProcessMemoryError::ProcessExited);
        }
        let (creation, _, _, _) = self
            .process
            .GetProcessTimes()
            .map_err(|_| ProcessMemoryError::SamplingFailed)?;
        self.identity
            .verify(self.identity.pid, filetime_value(creation))?;
        let counters = self
            .process
            .GetProcessMemoryInfo()
            .map_err(|_| ProcessMemoryError::SamplingFailed)?;
        Ok(finish_sample(
            self.identity,
            RawMemory {
                resident_bytes: usize_to_u64(counters.WorkingSetSize)?,
                peak_resident_bytes: Some(usize_to_u64(counters.PeakWorkingSetSize)?),
                private_bytes: Some(usize_to_u64(counters.PrivateUsage)?),
                physical_footprint_bytes: None,
            },
        ))
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "windows"
)))]
impl ProcessMemoryObserver {
    pub(crate) fn capture(pid: u32) -> Result<Self, ProcessMemoryError> {
        let identity = ProcessIdentity {
            pid,
            start_marker: 0,
        };
        if identity.pid == 0 {
            return Err(ProcessMemoryError::InvalidPid);
        }
        Err(ProcessMemoryError::PlatformUnsupported)
    }

    pub(crate) fn sample(&self) -> Result<ProcessMemorySample, ProcessMemoryError> {
        let _ = self.identity;
        Err(ProcessMemoryError::PlatformUnsupported)
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_start_time(stat: &str) -> Option<(u32, u64, &str)> {
    let pid = stat.split_whitespace().next()?.parse().ok()?;
    let close = stat.rfind(")")?;
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    // Field 3 (state) precedes fields 4..22; field 22 is starttime.
    let state = rest.next()?;
    let start_marker = rest.nth(18)?.parse().ok()?;
    Some((pid, start_marker, state))
}

#[cfg(target_os = "linux")]
fn linux_live_identity(stat: &str) -> Result<(u32, u64), ProcessMemoryError> {
    let (pid, start_marker, state) =
        linux_start_time(stat).ok_or(ProcessMemoryError::SamplingFailed)?;
    if matches!(state, "Z" | "X" | "x") {
        return Err(ProcessMemoryError::ProcessExited);
    }
    Ok((pid, start_marker))
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
struct LinuxMemoryStatus {
    resident_bytes: u64,
    peak_resident_bytes: Option<u64>,
}

#[cfg(any(target_os = "linux", test))]
fn linux_memory_status(text: &str) -> Result<LinuxMemoryStatus, ProcessMemoryError> {
    let mut resident_bytes = None;
    let mut peak_resident_bytes = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let target = match key {
            "VmRSS" => &mut resident_bytes,
            "VmHWM" => &mut peak_resident_bytes,
            _ => continue,
        };
        let mut parts = value.split_whitespace();
        let number = parts
            .next()
            .ok_or(ProcessMemoryError::SamplingFailed)?
            .parse::<u64>()
            .map_err(|_| ProcessMemoryError::SamplingFailed)?;
        if parts.next() != Some("kB") || parts.next().is_some() {
            return Err(ProcessMemoryError::SamplingFailed);
        }
        *target = Some(
            number
                .checked_mul(1024)
                .ok_or(ProcessMemoryError::SamplingFailed)?,
        );
    }
    let resident_bytes = resident_bytes.ok_or(ProcessMemoryError::ProcessExited)?;
    Ok(LinuxMemoryStatus {
        resident_bytes,
        peak_resident_bytes,
    })
}

#[cfg(target_os = "linux")]
fn io_failure(error: &std::io::Error) -> ProcessMemoryError {
    if error.kind() == std::io::ErrorKind::NotFound
        || error.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32)
    {
        ProcessMemoryError::ProcessExited
    } else {
        ProcessMemoryError::SamplingFailed
    }
}

#[cfg(target_os = "macos")]
fn macos_usage(
    pid: u32,
) -> Result<libproc::pid_rusage::RUsageInfoV2, ProcessMemoryError> {
    let signed_pid = i32::try_from(pid).map_err(|_| ProcessMemoryError::InvalidPid)?;
    libproc::pid_rusage::pidrusage::<libproc::pid_rusage::RUsageInfoV2>(signed_pid)
        .map_err(|_| ProcessMemoryError::SamplingFailed)
}

#[cfg(target_os = "windows")]
fn filetime_value(time: winsafe::FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

#[cfg(target_os = "windows")]
fn usize_to_u64(value: usize) -> Result<u64, ProcessMemoryError> {
    u64::try_from(value).map_err(|_| ProcessMemoryError::SamplingFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_pid_is_rejected() {
        assert!(matches!(
            ProcessMemoryObserver::capture(0),
            Err(ProcessMemoryError::InvalidPid)
        ));
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn start_marker_mismatch_is_rejected_as_identity_change() {
        let identity = ProcessIdentity {
            pid: 42,
            start_marker: 7,
        };
        assert_eq!(
            identity.verify(42, 8),
            Err(ProcessMemoryError::IdentityChanged)
        );
        assert_eq!(
            identity.verify(43, 7),
            Err(ProcessMemoryError::IdentityChanged)
        );
        assert_eq!(identity.verify(42, 7), Ok(()));
    }

    #[cfg(any(target_os = "linux", test))]
    #[test]
    fn proc_stat_start_time_handles_command_names_with_spaces_and_parentheses() {
        let line = "4242 (bend lint (worker)) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 12345 21 22";
        assert_eq!(linux_start_time(line), Some((4242, 12345, "S")));
    }

    #[cfg(any(target_os = "linux", test))]
    #[test]
    fn proc_stat_rejects_truncated_or_non_numeric_start_time() {
        assert_eq!(linux_start_time("4242 (worker) S 1 2"), None);
        assert_eq!(
            linux_start_time(
                "4242 (worker) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 nope"
            ),
            None
        );
    }

    #[cfg(any(target_os = "linux", test))]
    #[test]
    fn proc_status_preserves_units_and_never_infers_missing_resident_memory() {
        let status = linux_memory_status("VmRSS:\t13 kB\nVmHWM:\t21 kB\nRssAnon:\t9 kB\n")
            .expect("valid procfs status");
        assert_eq!(status.resident_bytes, 13 * 1024);
        assert_eq!(status.peak_resident_bytes, Some(21 * 1024));

        for text in [
            "VmRSS: 5 MB\n",
            "VmRSS: 5 kB trailing\n",
            "VmRSS: 18446744073709551615 kB\n",
            "VmRSS: -1 kB\n",
        ] {
            assert_eq!(
                linux_memory_status(text),
                Err(ProcessMemoryError::SamplingFailed)
            );
        }
        for text in ["Name: gone\n", "VmHWM: 21 kB\n"] {
            assert_eq!(
                linux_memory_status(text),
                Err(ProcessMemoryError::ProcessExited)
            );
        }
    }

    #[cfg(target_os = "linux")]
    fn fixture_stat(start_marker: u64) -> String {
        format!("4242 (owned compiler) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 {start_marker} 20 21\n")
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sample_rejects_reused_pid_after_reading_other_process_memory()
    -> Result<(), Box<dyn std::error::Error>> {
        let proc = tempfile::tempdir()?;
        let stat = proc.path().join("stat");
        std::fs::write(&stat, fixture_stat(101))?;
        std::fs::write(proc.path().join("status"), "VmRSS: 900 kB\nVmHWM: 1200 kB\n")?;
        let identity = ProcessIdentity { pid: 4242, start_marker: 101 };
        let result = linux_sample_with(identity, |name| {
            let text = std::fs::read_to_string(proc.path().join(name))?;
            if name == "status" {
                std::fs::write(&stat, fixture_stat(202))?;
            }
            Ok(text)
        });
        assert_eq!(result, Err(ProcessMemoryError::IdentityChanged));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sample_rejects_exit_after_reading_memory()
    -> Result<(), Box<dyn std::error::Error>> {
        let proc = tempfile::tempdir()?;
        let stat = proc.path().join("stat");
        std::fs::write(&stat, fixture_stat(101))?;
        std::fs::write(proc.path().join("status"), "VmRSS: 900 kB\n")?;
        let identity = ProcessIdentity { pid: 4242, start_marker: 101 };
        let result = linux_sample_with(identity, |name| {
            let text = std::fs::read_to_string(proc.path().join(name))?;
            if name == "status" {
                std::fs::remove_file(&stat)?;
            }
            Ok(text)
        });
        assert_eq!(result, Err(ProcessMemoryError::ProcessExited));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sample_rejects_zombie_after_reading_memory()
    -> Result<(), Box<dyn std::error::Error>> {
        let proc = tempfile::tempdir()?;
        let stat = proc.path().join("stat");
        std::fs::write(&stat, fixture_stat(101))?;
        std::fs::write(proc.path().join("status"), "VmRSS: 900 kB\n")?;
        let identity = ProcessIdentity { pid: 4242, start_marker: 101 };
        let result = linux_sample_with(identity, |name| {
            let text = std::fs::read_to_string(proc.path().join(name))?;
            if name == "status" {
                std::fs::write(&stat, fixture_stat(101).replacen(") S ", ") Z ", 1))?;
            }
            Ok(text)
        });
        assert_eq!(result, Err(ProcessMemoryError::ProcessExited));
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_sample_retains_original_proc_directory_after_pid_path_replaced()
    -> Result<(), Box<dyn std::error::Error>> {
        let proc = tempfile::tempdir()?;
        let original = proc.path().join("4242");
        std::fs::create_dir(&original)?;
        std::fs::write(original.join("stat"), fixture_stat(101))?;
        std::fs::write(original.join("status"), "VmRSS: 13 kB\nVmHWM: 21 kB\n")?;
        let observer = ProcessMemoryObserver::capture_linux_at(proc.path(), 4242)?;
        std::fs::rename(&original, proc.path().join("old-process"))?;
        std::fs::create_dir(&original)?;
        std::fs::write(original.join("stat"), fixture_stat(202))?;
        std::fs::write(original.join("status"), "VmRSS: 900 kB\nVmHWM: 1200 kB\n")?;

        let sample = observer.sample()?;
        assert_eq!(sample.pid, 4242);
        assert_eq!(sample.resident_bytes, Some(13 * 1024));
        assert_eq!(sample.peak_resident_bytes, Some(21 * 1024));
        assert_eq!(sample.private_bytes, None);
        assert!(sample.unavailable.iter().any(|field| field.metric == "privateBytes"));
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn samples_owned_process_identity_and_resident_memory()
    -> Result<(), Box<dyn std::error::Error>> {
        let pid = std::process::id();
        let observer = ProcessMemoryObserver::capture(pid)?;
        let first = observer.sample()?;
        assert_eq!(first.pid, pid);
        assert!(first.resident_bytes.is_some_and(|bytes| bytes > 0));
        assert!(!first.scope.is_empty());
        assert!(!first.definitions.is_empty());

        let second = observer.sample()?;
        assert_eq!(second.pid, pid);
        assert!(second.resident_bytes.is_some_and(|bytes| bytes > 0));
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn unavailable_metrics_are_explicit_and_never_replace_resident_memory()
    -> Result<(), Box<dyn std::error::Error>> {
        let observer = ProcessMemoryObserver::capture(std::process::id())?;
        let sample = observer.sample()?;
        for metric in &sample.unavailable {
            assert!(!metric.metric.is_empty());
            assert!(!metric.reason.is_empty());
            assert_ne!(metric.metric, "residentBytes");
        }

        #[cfg(target_os = "linux")]
        {
            assert!(
                sample
                    .unavailable
                    .iter()
                    .any(|metric| metric.metric == "privateBytes")
            );
            assert!(
                sample
                    .unavailable
                    .iter()
                    .any(|metric| metric.metric == "physicalFootprintBytes")
            );
        }
        #[cfg(target_os = "macos")]
        {
            assert!(
                sample
                    .unavailable
                    .iter()
                    .any(|metric| metric.metric == "peakResidentBytes")
            );
            assert!(
                sample
                    .unavailable
                    .iter()
                    .any(|metric| metric.metric == "privateBytes")
            );
            assert!(
                sample
                    .unavailable
                    .iter()
                    .all(|metric| metric.metric != "physicalFootprintBytes")
            );
        }
        #[cfg(target_os = "windows")]
        {
            assert!(
                sample
                    .unavailable
                    .iter()
                    .any(|metric| metric.metric == "physicalFootprintBytes")
            );
            assert!(
                sample
                    .unavailable
                    .iter()
                    .all(|metric| metric.metric != "privateBytes")
            );
        }
        Ok(())
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    #[test]
    fn sample_serializes_camel_case_fields_and_definitions()
    -> Result<(), Box<dyn std::error::Error>> {
        let observer = ProcessMemoryObserver::capture(std::process::id())?;
        let sample = observer.sample()?;
        let value = serde_json::to_value(&sample)?;
        assert!(value["residentBytes"].is_u64());
        assert!(value["scope"].is_string());
        assert!(value["unavailable"].is_array());
        assert!(value["definitions"].is_array());
        assert!(value.get("resident_bytes").is_none());
        for definition in sample.definitions {
            assert_eq!(definition.unit, "bytes");
            assert!(!definition.metric.is_empty());
            assert!(!definition.source.is_empty());
            assert!(!definition.definition.is_empty());
        }
        Ok(())
    }
}
