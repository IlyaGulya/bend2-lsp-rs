use crate::ToolResult;
use serde_json::{Value, json};

/// Observes only the supplied external PID; no system-wide process enumeration.
pub(crate) struct MemoryObserver {
    pid: u32,
    #[cfg(target_os = "linux")]
    status_path: String,
    #[cfg(target_os = "macos")]
    process_start: u64,
    #[cfg(target_os = "windows")]
    process: winsafe::guard::CloseHandleGuard<winsafe::HPROCESS>,
}

impl MemoryObserver {
    pub(crate) fn new(pid: u32) -> ToolResult<Self> {
        if pid == 0 {
            return Err("Process memory requires a nonzero external PID".into());
        }
        #[cfg(target_os = "linux")]
        {
            let status_path = format!("/proc/{pid}/status");
            std::fs::File::open(&status_path)
                .map_err(|error| format!("Opening memory status for PID {pid}: {error}"))?;
            Ok(Self { pid, status_path })
        }
        #[cfg(target_os = "macos")]
        {
            let info = macos_usage(pid)?;
            if info.ri_proc_exit_abstime != 0 {
                return Err(format!("Memory target PID {pid} has exited").into());
            }
            Ok(Self {
                pid,
                process_start: info.ri_proc_start_abstime,
            })
        }
        #[cfg(target_os = "windows")]
        {
            let process = winsafe::HPROCESS::OpenProcess(
                winsafe::co::PROCESS::QUERY_LIMITED_INFORMATION | winsafe::co::PROCESS::SYNCHRONIZE,
                false,
                pid,
            )
            .map_err(|error| format!("Opening memory target PID {pid}: {error}"))?;
            Ok(Self { pid, process })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            Err(format!("Process memory is unsupported on {}", std::env::consts::OS).into())
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
    pub(crate) fn sample(&mut self) -> ToolResult<Value> {
        #[cfg(target_os = "linux")]
        let mut value = {
            let text = std::fs::read_to_string(&self.status_path)
                .map_err(|error| format!("Reading memory status for PID {}: {error}", self.pid))?;
            linux_status(&text)
                .map_err(|error| format!("Parsing memory status for PID {}: {error}", self.pid))?
        };
        #[cfg(target_os = "macos")]
        let mut value = {
            let info = macos_usage(self.pid)?;
            if info.ri_proc_start_abstime != self.process_start || info.ri_proc_exit_abstime != 0 {
                return Err(
                    format!("Memory target PID {} exited or was replaced", self.pid).into(),
                );
            }
            json!({
                "rss_bytes": info.ri_resident_size,
                "kernel_high_watermark_bytes": null,
                "native_metrics": {
                    "macos_physical_footprint_bytes": info.ri_phys_footprint,
                    "macos_wired_bytes": info.ri_wired_size
                }
            })
        };
        #[cfg(target_os = "windows")]
        let mut value = {
            let state = self
                .process
                .WaitForSingleObject(Some(0))
                .map_err(|error| format!("Checking memory target PID {}: {error}", self.pid))?;
            if state != winsafe::co::WAIT::TIMEOUT {
                return Err(format!(
                    "Memory target PID {} is no longer running ({state})",
                    self.pid
                )
                .into());
            }
            let info = self
                .process
                .GetProcessMemoryInfo()
                .map_err(|error| format!("GetProcessMemoryInfo for PID {}: {error}", self.pid))?;
            json!({
                "rss_bytes": info.WorkingSetSize,
                "kernel_high_watermark_bytes": info.PeakWorkingSetSize,
                "native_metrics": {
                    "windows_working_set_bytes": info.WorkingSetSize,
                    "windows_peak_working_set_bytes": info.PeakWorkingSetSize,
                    "windows_private_commit_bytes": info.PrivateUsage,
                    "windows_peak_private_commit_bytes": info.PeakPagefileUsage,
                    "windows_paged_pool_bytes": info.QuotaPagedPoolUsage,
                    "windows_nonpaged_pool_bytes": info.QuotaNonPagedPoolUsage
                }
            })
        };
        value["pid"] = json!(self.pid);
        Ok(value)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    pub(crate) fn sample(&mut self) -> ToolResult<Value> {
        Err(format!(
            "Process memory for PID {} is unsupported on {}",
            self.pid,
            std::env::consts::OS
        )
        .into())
    }
}

#[cfg(target_os = "macos")]
fn macos_usage(pid: u32) -> ToolResult<libproc::pid_rusage::RUsageInfoV2> {
    let signed_pid = i32::try_from(pid)?;
    libproc::pid_rusage::pidrusage(signed_pid)
        .map_err(|error| format!("proc_pid_rusage for PID {pid}: {error}").into())
}

#[cfg(any(target_os = "linux", test))]
fn linux_status(text: &str) -> ToolResult<Value> {
    let mut result =
        json!({"rss_bytes": null, "kernel_high_watermark_bytes": null, "native_metrics": {}});
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let field = match key {
            "VmRSS" => "rss_bytes",
            "VmHWM" => "kernel_high_watermark_bytes",
            "RssAnon" => "linux_anonymous_rss_bytes",
            "RssFile" => "linux_file_rss_bytes",
            "RssShmem" => "linux_shared_memory_rss_bytes",
            "VmSwap" => "linux_swap_bytes",
            _ => continue,
        };
        let mut parts = value.split_whitespace();
        let number = parts
            .next()
            .ok_or("Missing procfs memory value")?
            .parse::<u64>()?;
        if parts.next() != Some("kB") || parts.next().is_some() {
            return Err(format!("Unexpected procfs memory unit for {key}").into());
        }
        let bytes = number.checked_mul(1024).ok_or("Procfs memory overflow")?;
        if matches!(key, "VmRSS" | "VmHWM") {
            result[field] = json!(bytes);
        } else {
            result["native_metrics"][field] = json!(bytes);
        }
    }
    if result["rss_bytes"].is_null() {
        return Err(
            "Procfs status has no VmRSS; process exited or resident memory is unavailable".into(),
        );
    }
    if result["kernel_high_watermark_bytes"].is_null() {
        result["limitations"] = json!(["VmHWM absent; kernel resident high watermark unavailable"]);
    }
    Ok(result)
}

/// Definitions travel with raw samples and report metadata, so unlike native
/// metrics are never mistaken for interchangeable resident or heap measurements.
fn definitions() -> (&'static str, Value, Value) {
    match std::env::consts::OS {
        "linux" => (
            "procfs status",
            json!({
                "rss_bytes": "bytes; /proc/PID/status VmRSS (RssAnon + RssFile + RssShmem); kernel approximate resident accounting, not heap allocations",
                "kernel_high_watermark_bytes": "bytes; VmHWM, process-lifetime peak resident set, not a phase delta; kernel approximate accounting",
                "linux_anonymous_rss_bytes": "bytes; RssAnon, resident anonymous mappings",
                "linux_file_rss_bytes": "bytes; RssFile, resident file-backed mappings",
                "linux_shared_memory_rss_bytes": "bytes; RssShmem, resident shared memory mappings",
                "linux_swap_bytes": "bytes; VmSwap, swapped anonymous private pages; excludes shared-memory swap"
            }),
            json!([]),
        ),
        "macos" => (
            "libproc proc_pid_rusage RUSAGE_INFO_V2",
            json!({
                "rss_bytes": "bytes; ri_resident_size, current resident memory; not physical footprint or heap live bytes",
                "kernel_high_watermark_bytes": "unavailable; RUSAGE_INFO_V2 does not expose a resident-set high watermark; never substituted with footprint",
                "macos_physical_footprint_bytes": "bytes; ri_phys_footprint, Darwin physical-footprint ledger charge; distinct from resident size and includes compressed-memory accounting",
                "macos_wired_bytes": "bytes; ri_wired_size, current wired memory"
            }),
            json!(["Resident-set kernel high watermark is not exposed by RUSAGE_INFO_V2"]),
        ),
        "windows" => (
            "PSAPI GetProcessMemoryInfo PROCESS_MEMORY_COUNTERS_EX",
            json!({
                "rss_bytes": "bytes; WorkingSetSize, current resident working set including shared pages; compatibility resident field, not private commit",
                "kernel_high_watermark_bytes": "bytes; PeakWorkingSetSize, process-lifetime peak resident working set, not a phase delta",
                "windows_working_set_bytes": "bytes; WorkingSetSize, current resident working set",
                "windows_peak_working_set_bytes": "bytes; PeakWorkingSetSize, process-lifetime peak working set",
                "windows_private_commit_bytes": "bytes; PrivateUsage, private committed memory (commit charge), not RSS or actual pagefile occupancy",
                "windows_peak_private_commit_bytes": "bytes; PeakPagefileUsage, process-lifetime peak private commit charge, not peak RSS or pagefile occupancy",
                "windows_paged_pool_bytes": "bytes; QuotaPagedPoolUsage, current paged kernel pool usage",
                "windows_nonpaged_pool_bytes": "bytes; QuotaNonPagedPoolUsage, current nonpaged kernel pool usage"
            }),
            json!([]),
        ),
        _ => (
            "unsupported",
            json!({}),
            json!(["No process-memory backend for this operating system"]),
        ),
    }
}

pub(crate) fn metadata() -> Value {
    let (backend, metrics, limitations) = definitions();
    json!({
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "backend": backend,
        "units": "bytes",
        "scope": "single external PID only; not children, system RAM, heap live bytes or allocations",
        "metrics": metrics,
        "limitations": limitations,
        "sampling": "20ms wait after each observation; actual cadence includes collection time; sampled peaks are lower bounds"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_units_and_native_metrics_preserve_resident_semantics() -> ToolResult<()> {
        let value = linux_status(
            "Name:\ttarget\nVmRSS:\t13 kB\nVmHWM:\t21 kB\nRssAnon:\t9 kB\nRssFile:\t3 kB\nRssShmem:\t1 kB\nVmSwap:\t7 kB\n",
        )?;
        assert_eq!(value["rss_bytes"], 13 * 1024);
        assert_eq!(value["kernel_high_watermark_bytes"], 21 * 1024);
        assert_eq!(value["native_metrics"]["linux_swap_bytes"], 7 * 1024);
        assert_eq!(
            value["native_metrics"]["linux_anonymous_rss_bytes"],
            9 * 1024
        );
        Ok(())
    }

    #[test]
    fn malformed_or_missing_resident_values_are_not_inferred() -> ToolResult<()> {
        for text in [
            "Name: gone\n",
            "VmRSS: 5 MB\n",
            "VmRSS: 5 kB trailing\n",
            "VmRSS: 18446744073709551615 kB\n",
            "VmRSS: -1 kB\n",
        ] {
            assert!(linux_status(text).is_err());
        }
        let value = linux_status("VmRSS: 0 kB\n")?;
        assert_eq!(value["rss_bytes"], 0);
        assert!(value["kernel_high_watermark_bytes"].is_null());
        assert!(
            value["limitations"]
                .as_array()
                .is_some_and(|values| !values.is_empty())
        );
        assert!(MemoryObserver::new(0).is_err());
        Ok(())
    }
}
