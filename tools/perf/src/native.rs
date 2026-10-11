use crate::{ToolResult, common};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) mod discovery;
mod memory;

pub(crate) fn validate_dhat_profile(
    path: &Path,
    pid: u32,
    command: &[String],
) -> ToolResult<Value> {
    memory::validate_dhat_profile(path, pid, command)
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    let (command, rest) = args
        .split_first()
        .ok_or("Expected native latency, discovery, or memory")?;
    match command.as_str() {
        "latency" => crate::latency::collect(rest),
        "discovery" => discovery::run(rest),
        "memory" => memory::run(rest),
        _ => Err(format!("Unknown native command: {command}").into()),
    }
}

fn identity(path: &Path) -> ToolResult<Value> {
    Ok(json!({"path": path.canonicalize()?, "sha256": common::sha256_file(path)?}))
}

fn verify_identity(item: &Value) -> ToolResult<()> {
    let path = Path::new(item["path"].as_str().ok_or("Missing input identity path")?);
    if identity(path)? != *item {
        return Err(format!("Build/collection input changed: {}", path.display()).into());
    }
    Ok(())
}

fn read_json(path: &Path) -> ToolResult<Value> {
    Ok(serde_json::from_reader(fs::File::open(path)?)?)
}

fn command_output(command: &[&str]) -> ToolResult<String> {
    let (program, arguments) = command.split_first().ok_or("Empty provenance command")?;
    let output = Command::new(program).args(arguments).output()?;
    if !output.status.success() {
        return Err(format!(
            "{} failed: {}",
            command.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn ci_metadata() -> Value {
    let values: BTreeMap<_, _> = [
        "GITHUB_RUN_ID",
        "GITHUB_RUN_ATTEMPT",
        "GITHUB_SHA",
        "GITHUB_WORKFLOW_REF",
        "GITHUB_JOB",
        "RUNNER_OS",
        "RUNNER_ARCH",
        "RUNNER_NAME",
        "ImageOS",
        "ImageVersion",
        "PERF_RUNNER",
        "PERF_TARGET",
    ]
    .into_iter()
    .map(|key| (key, env::var(key).ok()))
    .collect();
    json!(values)
}

fn hardware_metadata() -> Value {
    let mut result = json!({
        "logical_cpus": std::thread::available_parallelism().ok().map(std::num::NonZeroUsize::get),
        "platform": env::consts::OS, "machine": env::consts::ARCH,
    });
    if cfg!(target_os = "linux") {
        for (path, key) in [
            ("/proc/cpuinfo", "cpuinfo"),
            ("/proc/meminfo", "meminfo"),
            ("/etc/os-release", "os_release"),
        ] {
            result[key] = match fs::read_to_string(path) {
                Ok(text) => json!(if key == "cpuinfo" {
                    text.split("\n\n").next().unwrap_or("").to_owned()
                } else {
                    text
                }),
                Err(error) => json!({"unavailable": error.to_string()}),
            };
        }
    } else {
        let command = if cfg!(target_os = "macos") {
            vec![
                "sysctl",
                "-n",
                "machdep.cpu.brand_string",
                "hw.memsize",
                "hw.ncpu",
            ]
        } else {
            vec![
                "powershell.exe",
                "-NoProfile",
                "-Command",
                "@{Processor=@(Get-CimInstance Win32_Processor | Select-Object Name,NumberOfCores,NumberOfLogicalProcessors); Memory=(Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory; OS=(Get-CimInstance Win32_OperatingSystem | Select-Object Caption,Version,BuildNumber)} | ConvertTo-Json -Depth 4",
            ]
        };
        result["native_hardware"] = match command_output(&command) {
            Ok(output) => json!({"command": command, "output": output}),
            Err(error) => json!({"command": command, "unavailable": error.to_string()}),
        };
    }
    result
}

fn revision(value: &str) -> Result<String, String> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Must be an exact 40-character Git revision".to_owned());
    }
    Ok(value.to_ascii_lowercase())
}

fn positive(value: &str) -> Result<usize, String> {
    let number: usize = value.parse().map_err(|_| "Must be a positive integer")?;
    if number == 0 {
        return Err("Must be a positive integer".to_owned());
    }
    Ok(number)
}

fn executable_identity() -> ToolResult<Value> {
    identity(&env::current_exe()?)
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn command_line() -> Vec<String> {
    env::args().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_identity_rejects_modified_deleted_or_misdeclared_files() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("binary");
        fs::write(&path, b"original")?;
        let item = identity(&path)?;
        verify_identity(&item)?;
        let mut wrong_path = item.clone();
        wrong_path["path"] = json!(directory.path().join("different"));
        assert!(verify_identity(&wrong_path).is_err());
        fs::write(&path, b"modified")?;
        assert!(verify_identity(&item).is_err());
        fs::remove_file(&path)?;
        assert!(verify_identity(&item).is_err());
        Ok(())
    }
}
