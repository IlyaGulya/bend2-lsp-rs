use super::{
    APPLICATIONS, ci_metadata, command_line, heap, identity, jobs, lifecycle, read_json,
    verify_debug_sidecars, verify_identity, verify_provenance,
};
use crate::{ToolResult, common};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(super) fn collect(
    binary_dir: &Path,
    provenance_path: &Path,
    output_dir: &Path,
) -> ToolResult<()> {
    let target = common::native_target()?;
    if output_dir.exists() {
        return Err("Memory output directory exists; raw evidence is never replaced".into());
    }
    fs::create_dir_all(output_dir)?;
    let output = output_dir.canonicalize()?;
    let mut data = json!({"format_version": 1, "status": "running", "native_target": target,
        "command": command_line(), "ci": ci_metadata(), "provenance": null,
        "measurement": "DHAT Rust allocator events, not RSS; candidate only, report-only numerical values", "profiles": []});
    persist(&output, &data)?;
    let outcome = collect_profiles(binary_dir, provenance_path, &output, &mut data, target);
    match &outcome {
        Ok(()) => data["status"] = json!("complete"),
        Err(error) => {
            data["status"] = json!("failed");
            data["error"] = json!(error.to_string());
        }
    }
    persist(&output, &data)?;
    outcome
}

fn binaries(binary_dir: &Path, provenance: &Value) -> ToolResult<BTreeMap<&'static str, PathBuf>> {
    let expected = provenance["binary_and_symbol_files"]
        .as_object()
        .ok_or("Missing binary provenance")?;
    let commands = provenance["commands"]
        .as_str()
        .ok_or("Missing build command provenance")?;
    let packed = commands.contains("CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed")
        || provenance["build_environment"]["CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO"] == "packed"
        || provenance["build_command_environment"]["CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO"]
            == "packed";
    let mut binaries = BTreeMap::new();
    for name in APPLICATIONS {
        let suffix = if cfg!(target_os = "windows") {
            ".exe"
        } else {
            ""
        };
        let path = binary_dir.join(format!("{name}{suffix}")).canonicalize()?;
        let item = expected
            .get(path.to_str().ok_or("Non-Unicode profile binary path")?)
            .ok_or("Profile binary missing from build provenance")?;
        if identity(&path)? != *item {
            return Err(format!("Profile binary changed: {}", path.display()).into());
        }
        verify_debug_sidecars(&path, expected, packed)?;
        binaries.insert(name, path);
    }
    Ok(binaries)
}

fn collect_profiles(
    binary_dir: &Path,
    provenance_path: &Path,
    output: &Path,
    data: &mut Value,
    target: &str,
) -> ToolResult<()> {
    data["provenance"] = read_json(provenance_path)?;
    data["provenance_file"] = identity(provenance_path)?;
    persist(output, data)?;
    verify_provenance(&data["provenance"], target)?;
    let binaries = binaries(binary_dir, &data["provenance"])?;
    let mut failed = false;
    for (name, binary_name, arguments) in jobs() {
        let directory = output.join(&name);
        fs::create_dir(&directory)?;
        let profile = directory.join("dhat-heap.json");
        let binary = binaries.get(binary_name).ok_or("Missing profile binary")?;
        let mut command = vec![binary.to_str().ok_or("Non-Unicode binary path")?.to_owned()];
        command.extend(arguments);
        let item = json!({"name": name, "status": "running", "command": command,
            "child_env": {"BEND2_LSP_DHAT_FILE": profile}});
        data["profiles"]
            .as_array_mut()
            .ok_or("Missing profiles")?
            .push(item);
        persist(output, data)?;
        let item = data["profiles"]
            .as_array_mut()
            .and_then(|items| items.last_mut())
            .ok_or("Missing current profile")?;
        let outcome = collect_job(
            binary,
            &command,
            &directory,
            &profile,
            binary_name == "bend2-lsp",
            item,
        );
        match outcome {
            Ok(()) => item["status"] = json!("complete"),
            Err(error) => {
                item["status"] = json!("failed");
                item["error"] = json!(error.to_string());
                failed = true;
            }
        }
        persist(output, data)?;
    }
    verify_identity(&data["provenance_file"])?;
    verify_provenance(&data["provenance"], target)?;
    if failed {
        return Err("One or more of the 13 candidate allocation profiles failed; partial raw evidence retained".into());
    }
    let profiles = data["profiles"].as_array().ok_or("Missing profiles")?;
    if profiles.len() != 13 || profiles.iter().any(|item| item["status"] != "complete") {
        return Err("Missing complete 13-profile candidate allocation evidence".into());
    }
    Ok(())
}

fn collect_job(
    binary: &Path,
    command: &[String],
    directory: &Path,
    profile: &Path,
    lsp: bool,
    item: &mut Value,
) -> ToolResult<()> {
    let pid = if lsp {
        let pid = lifecycle::collect(binary, profile, directory)?;
        item["semantics"] = json!(
            "initialize; open 4 buffers including analyzer_large; correct hover/definition; dependency U32->U64 causal hover; diagnostics; graceful shutdown/exit"
        );
        pid
    } else {
        item["cwd"] = json!(directory);
        run_example(command, directory, profile)?
    };
    item["pid"] = json!(pid);
    let summary = heap::validate_profile(profile, pid, command)?;
    for (key, value) in summary.as_object().ok_or("Missing allocation summary")? {
        item[key] = value.clone();
    }
    Ok(())
}

struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_example(command: &[String], directory: &Path, profile: &Path) -> ToolResult<u32> {
    let (binary, arguments) = command.split_first().ok_or("Empty profile command")?;
    let mut process = Command::new(binary);
    process
        .args(arguments)
        .current_dir(directory)
        .stdin(Stdio::null());
    process
        .stdout(fs::File::create(directory.join("stdout.txt"))?)
        .stderr(fs::File::create(directory.join("stderr.txt"))?);
    for key in [
        "BEND_LIB",
        "BEND2_LSP_COMPILER_METRICS_FILE",
        "BEND2_LSP_DHAT_FILE",
        "BEND2_LSP_TRACE",
    ] {
        process.env_remove(key);
    }
    process.env("BEND2_LSP_DHAT_FILE", profile);
    let mut child = OwnedChild(process.spawn()?);
    let pid = child.0.id();
    let started = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                return Err(format!("Profile process exited with status {status}").into());
            }
            return Ok(pid);
        }
        if started.elapsed() >= Duration::from_secs(300) {
            return Err("Profile process exceeded 300-second timeout; killed and reaped".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn persist(output: &Path, data: &Value) -> ToolResult<()> {
    common::write_json(&output.join("report.json"), data)?;
    let mut lines = format!(
        "## Native DHAT heap allocation evidence — {}\n\nCandidate-only separate symbolized optimized release build. Allocator bytes/blocks are not RSS. Numeric results are report-only; missing, malformed, unsymbolized profiles or failed semantics fail CI.\n\nCollection status: **{}**.\n\n| Workload | Total allocated bytes | Blocks | Peak live bytes | Status |\n|---|---:|---:|---:|---|\n",
        data["native_target"].as_str().unwrap_or("unknown"),
        data["status"].as_str().unwrap_or("unknown")
    );
    for item in data["profiles"].as_array().ok_or("Missing profiles")? {
        let cell = |key: &str| {
            item[key]
                .as_u64()
                .map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
        };
        writeln!(
            lines,
            "| {} | {} | {} | {} | {} |",
            item["name"].as_str().unwrap_or("unknown"),
            cell("total_allocated_bytes"),
            cell("total_allocated_blocks"),
            cell("global_peak_live_bytes"),
            item["status"].as_str().unwrap_or("unknown")
        )?;
        if let Some(error) = item["error"].as_str() {
            writeln!(
                lines,
                "\n{} failure (partial raw evidence retained):\n```text\n{error}\n```",
                item["name"].as_str().unwrap_or("unknown")
            )?;
        }
    }
    if let Some(error) = data["error"].as_str() {
        writeln!(
            lines,
            "\nCollection failure (partial raw evidence retained):\n```text\n{error}\n```"
        )?;
    }
    fs::write(output.join("report.md"), lines)?;
    Ok(())
}
