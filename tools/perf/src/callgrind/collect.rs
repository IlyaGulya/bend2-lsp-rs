//! Process execution and immutable source handling shared by Callgrind collectors.
use super::{
    BENCHMARKS, baseline_name, parse_executable, parse_measurement, parse_workload_list, require,
    source_manifest,
};
use crate::{
    ToolResult, callgrind,
    common::{sha256_file, write_json},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub(crate) type Environment = BTreeMap<OsString, OsString>;

pub(crate) fn clean_environment() -> Environment {
    env::vars_os()
        .filter(|(key, _)| {
            !key.to_string_lossy().starts_with("IAI_CALLGRIND_") && key != "CARGO_TARGET_DIR"
        })
        .collect()
}

pub(crate) fn run_logged(
    command: &[OsString],
    cwd: &Path,
    stdout: &Path,
    stderr: &Path,
    environment: &Environment,
) -> ToolResult<String> {
    if let Some(parent) = stdout.parent() {
        fs::create_dir_all(parent)?;
    }
    let argv = command
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .ok_or("calibration command argument is not UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    write_json(
        &stdout.with_extension("command.json"),
        &json!({"argv": argv, "cwd": cwd}),
    )?;
    let (executable, arguments) = command.split_first().ok_or("empty calibration command")?;
    let status = Command::new(executable)
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .stdin(Stdio::null())
        .stdout(File::create(stdout)?)
        .stderr(File::create(stderr)?)
        .status()?;
    require(
        status.success(),
        &format!(
            "command exited {status}: {command:?}; see {} and {}",
            stderr.display(),
            stdout.display()
        ),
    )?;
    Ok(fs::read_to_string(stdout)?)
}

pub(crate) fn command_strings(command: &[&str]) -> Vec<OsString> {
    command.iter().map(OsString::from).collect()
}

pub(crate) fn environment_metadata(
    source: &Path,
    output: &Path,
    environment: &Environment,
) -> ToolResult<Value> {
    let mut tools = serde_json::Map::new();
    for (key, command) in [
        ("rustc", vec!["rustc", "--version", "--verbose"]),
        ("cargo", vec!["cargo", "--version"]),
        ("valgrind", vec!["valgrind", "--version"]),
    ] {
        let text = run_logged(
            &command_strings(&command),
            source,
            &output.join(format!("environment/{key}.stdout")),
            &output.join(format!("environment/{key}.stderr")),
            environment,
        )?;
        tools.insert(key.to_owned(), text.trim().into());
    }
    let installed = run_logged(
        &command_strings(&["cargo", "install", "--list"]),
        source,
        &output.join("environment/installed.stdout"),
        &output.join("environment/installed.stderr"),
        environment,
    )?;
    let versions: Vec<_> = installed
        .lines()
        .filter_map(|line| {
            line.strip_prefix("iai-callgrind-runner v")
                .and_then(|version| version.strip_suffix(':'))
        })
        .collect();
    let path = environment
        .get(&OsString::from("PATH"))
        .ok_or("runner PATH is missing")?;
    require(
        versions == ["0.16.1"]
            && env::split_paths(path)
                .any(|directory| directory.join("iai-callgrind-runner").is_file()),
        "requires cargo-installed iai-callgrind-runner 0.16.1 on PATH",
    )?;
    tools.insert(
        "iai_runner".to_owned(),
        "iai-callgrind-runner 0.16.1".into(),
    );
    let os = match env::consts::OS {
        "linux" => "Linux",
        "macos" => "Darwin",
        "windows" => "Windows",
        other => other,
    };
    tools.insert("os".to_owned(), os.into());
    tools.insert("arch".to_owned(), env::consts::ARCH.into());
    tools.insert("cache_args".to_owned(), json!(["--cache-sim=yes"]));
    Ok(Value::Object(tools))
}

pub(crate) fn host_metadata() -> ToolResult<Value> {
    let kernel = Command::new("uname").arg("-r").output()?;
    let hostname = Command::new("hostname").output()?;
    let mut host = json!({"kernel": String::from_utf8(kernel.stdout)?.trim(), "hostname": String::from_utf8(hostname.stdout)?.trim(), "cpu": "", "runner_identity": env::var("RUNNER_NAME").unwrap_or_else(|_| "unknown".to_owned())});
    if Path::new("/proc/cpuinfo").is_file() {
        host["cpuinfo"] = fs::read_to_string("/proc/cpuinfo")?.into();
    }
    Ok(host)
}

pub(crate) fn freeze_source(checkout: &Path, manifest: &Value) -> ToolResult<()> {
    for entry in manifest["files"]
        .as_array()
        .ok_or("manifest files missing")?
    {
        let path = checkout.join(entry["path"].as_str().ok_or("manifest path missing")?);
        let mut permissions = fs::metadata(&path)?.permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            permissions.set_mode(permissions.mode() & !0o222);
        }
        #[cfg(not(unix))]
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}
pub(crate) fn copy_verified_source(
    source: &Path,
    checkout: &Path,
    original: &Value,
) -> ToolResult<()> {
    fs::create_dir(checkout)?;
    for entry in original["files"]
        .as_array()
        .ok_or("manifest files missing")?
    {
        let relative = entry["path"].as_str().ok_or("manifest path missing")?;
        let destination = checkout.join(relative);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source.join(relative), destination)?;
    }
    require(
        source_manifest(checkout, &[])? == *original,
        "source changed while copying; discard this measurement",
    )
}
pub(crate) struct BenchmarkBuild {
    pub(crate) executable: PathBuf,
    pub(crate) identities: BTreeSet<super::Identity>,
    pub(crate) metadata: Value,
}

pub(crate) struct Build {
    pub(crate) checkout: PathBuf,
    pub(crate) manifest: Value,
    pub(crate) benchmarks: BTreeMap<&'static str, BenchmarkBuild>,
    pub(crate) identities: BTreeSet<super::Identity>,
    pub(crate) metadata: Value,
}

pub(crate) fn compile(
    checkout: PathBuf,
    variant: &str,
    output: &Path,
    environment: &Environment,
) -> ToolResult<Build> {
    let manifest = source_manifest(&checkout, &[])?;
    write_json(&output.join(format!("manifests/{variant}.json")), &manifest)?;
    freeze_source(&checkout, &manifest)?;
    let target = checkout.join("target");
    let mut build_environment = environment.clone();
    build_environment.insert("CARGO_TARGET_DIR".into(), target.as_os_str().to_owned());
    let mut command = command_strings(&[
        "cargo",
        "bench",
        "--locked",
        "--bench",
        "analysis",
        "--bench",
        "reference_locations",
        "--no-run",
        "--message-format=json",
        "--target-dir",
    ]);
    command.push(target.as_os_str().to_owned());
    let compiler_output = run_logged(
        &command,
        &checkout,
        &output.join(format!("builds/{variant}.stdout")),
        &output.join(format!("builds/{variant}.stderr")),
        &build_environment,
    )?;
    let mut benchmarks = BTreeMap::<_, BenchmarkBuild>::new();
    let mut identities = BTreeSet::new();
    let mut metadata = json!({"source_sha256": manifest["sha256"], "source_manifest": format!("manifests/{variant}.json"), "benchmarks": {}});
    for benchmark in BENCHMARKS {
        let executable = parse_executable(&compiler_output, &checkout, &target, benchmark)?;
        require(
            benchmarks
                .values()
                .all(|compiled| compiled.executable != executable),
            "distinct benchmark targets must not share an executable",
        )?;
        let mut permissions = fs::metadata(&executable)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&executable, permissions)?;
        let binary_sha256 = sha256_file(&executable)?;
        let listed = run_logged(
            &[executable.as_os_str().to_owned(), "--list".into()],
            &checkout,
            &output.join(format!("builds/{variant}/{benchmark}.list.stdout")),
            &output.join(format!("builds/{variant}/{benchmark}.list.stderr")),
            environment,
        )?;
        let listed_identities = parse_workload_list(&listed, benchmark)?;
        for identity in &listed_identities {
            require(
                identities.insert(identity.clone()),
                "benchmark workload identities collide across executables",
            )?;
        }
        let workloads: Vec<_> = listed_identities
            .iter()
            .map(|(function_name, id)| json!({"function_name": function_name, "id": id}))
            .collect();
        let benchmark_metadata = json!({"binary_sha256": binary_sha256, "executable": executable, "workloads": workloads,
            "workload_list": format!("builds/{variant}/{benchmark}.list.stdout")});
        metadata["benchmarks"][benchmark] = benchmark_metadata.clone();
        benchmarks.insert(
            benchmark,
            BenchmarkBuild {
                executable,
                identities: listed_identities,
                metadata: benchmark_metadata,
            },
        );
    }
    let build = Build {
        checkout,
        manifest,
        benchmarks,
        identities,
        metadata,
    };
    ensure_immutable(&build)?;
    Ok(build)
}

pub(crate) fn ensure_immutable(build: &Build) -> ToolResult<()> {
    require(
        source_manifest(&build.checkout, &[])? == build.manifest,
        "immutable source changed before/during measurement",
    )?;
    for benchmark in build.benchmarks.values() {
        require(
            sha256_file(&benchmark.executable)? == benchmark.metadata["binary_sha256"],
            "immutable compiled executable changed before/during measurement",
        )?;
    }
    Ok(())
}

pub(crate) fn measure(
    build: &Build,
    job_id: &str,
    pair: u64,
    variant: &str,
    order: usize,
    output: &Path,
    environment: &Environment,
) -> ToolResult<Value> {
    ensure_immutable(build)?;
    let name = format!("pair{pair:03}{}", variant.replace('_', ""));
    let raw = output.join("raw").join(name);
    fs::create_dir_all(&raw)?;
    let baseline = baseline_name(job_id, pair, variant);
    let mut workloads = Vec::with_capacity(build.identities.len());
    let mut benchmark_samples = serde_json::Map::new();
    for benchmark in BENCHMARKS {
        let compiled = &build.benchmarks[benchmark];
        let benchmark_raw = raw.join(benchmark);
        fs::create_dir(&benchmark_raw)?;
        let home = benchmark_raw.join("iai");
        let stdout = benchmark_raw.join("stdout.jsonl");
        let stderr = benchmark_raw.join("stderr.log");
        let command = vec![
            compiled.executable.as_os_str().to_owned(),
            "--output-format=json".into(),
            "--save-summary=pretty-json".into(),
            format!("--save-baseline={baseline}").into(),
            "--callgrind-args=--cache-sim=yes".into(),
            format!("--home={}", home.display()).into(),
        ];
        let measured = run_logged(&command, &build.checkout, &stdout, &stderr, environment)?;
        let (measured_workloads, summaries) = parse_measurement(
            &measured,
            &baseline,
            &compiled.executable,
            &compiled.identities,
        )?;
        let mut profile_count = 0_u64;
        for summary in summaries {
            let paths = summary["profiles"][0]["out_paths"]
                .as_array()
                .ok_or("executed summary has no raw Callgrind profiles")?;
            require(
                !paths.is_empty(),
                "executed summary has no raw Callgrind profiles",
            )?;
            for emitted in paths {
                let emitted = emitted.as_str().ok_or("malformed Callgrind output path")?;
                let path = callgrind::resolve_path(Path::new(emitted))?;
                require(
                    path.starts_with(&home) && path.is_file(),
                    "Callgrind output escaped fresh measurement home or is missing",
                )?;
                callgrind::check_probe_absent(&fs::read_to_string(path)?)?;
                profile_count += 1;
            }
        }
        workloads.extend(measured_workloads);
        benchmark_samples.insert(
            benchmark.to_owned(),
            json!({"raw_stdout": stdout.strip_prefix(output)?, "raw_stderr": stderr.strip_prefix(output)?, "verified_callgrind_profiles": profile_count}),
        );
    }
    ensure_immutable(build)?;
    let sample = json!({"pair": pair, "variant": variant, "order": order, "workloads": workloads, "benchmarks": benchmark_samples});
    write_json(&raw.join("sample.json"), &sample)?;
    Ok(sample)
}
