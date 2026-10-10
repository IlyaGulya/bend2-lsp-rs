use super::{
    CollectArgs, PROBE, VARIANTS, baseline_name,
    integrity::{
        self, parse_executable, parse_measurement, parse_probe_symbols, parse_workload_list,
        source_manifest,
    },
    require,
};
use crate::{
    ToolResult,
    common::{sha256_file, write_json},
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::OsString,
    fmt::Write as _,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const INLAY_ANCHOR: &str = "fn inlay_hints_warm(fixture: WarmSnapshot) -> Vec<analysis::InlayHint> {\n    std::hint::black_box(analysis::inlay_hints(\n        std::hint::black_box(fixture.snapshot),\n        analysis::TextRange::new(0, fixture.source.len()),\n    ))\n}";
const SETUP_ANCHORS: [&str; 3] = [
    "    fn small() -> Self {\n        Self {\n            source: SOURCE,\n            snapshot: &SMALL_SNAPSHOT,\n            completion_prefix: \"transform_\",\n            identifier_name: \"transform_31\",\n        }\n    }",
    "    fn medium() -> Self {\n        Self {\n            source: MEDIUM_SOURCE,\n            snapshot: &MEDIUM_SNAPSHOT,\n            completion_prefix: \"worker_\",\n            identifier_name: \"worker_0199\",\n        }\n    }",
    "    fn large() -> Self {\n        Self {\n            source: LARGE_SOURCE,\n            snapshot: &LARGE_SNAPSHOT,\n            completion_prefix: \"worker_\",\n            identifier_name: \"worker_0599\",\n        }\n    }",
];
const HELPERS: &str = "#[inline(never)]\nfn calibration_inlay_query(\n    snapshot: &analysis::DocumentSnapshot,\n    source_len: usize,\n) -> Vec<analysis::InlayHint> {\n    std::hint::black_box(analysis::inlay_hints(\n        std::hint::black_box(snapshot),\n        analysis::TextRange::new(0, source_len),\n    ))\n}\n\n#[inline(never)]\nfn calibration_allocation_control() {\n    std::hint::black_box(vec![1_u64; 65_536]);\n}\n\n";

type Environment = BTreeMap<OsString, OsString>;

fn exact_replace(text: &str, anchor: &str, replacement: &str) -> ToolResult<String> {
    require(
        text.matches(anchor).count() == 1,
        "calibration anchor must occur exactly once",
    )?;
    Ok(text.replacen(anchor, replacement, 1))
}

pub(super) fn variant_harness(text: &str, variant: &str) -> ToolResult<String> {
    require(VARIANTS.contains(&variant), "unknown calibration variant")?;
    // Rust source treats CRLF as LF; canonicalize before counting exact anchors so
    // checkout newlines cannot hide drift or mixed-newline duplicate anchors.
    let mut text = text.replace("\r\n", "\n");
    require(
        !text.contains(PROBE),
        "source harness already contains a calibration layout probe",
    )?;
    require(
        text.matches(INLAY_ANCHOR).count() == 1,
        "calibration anchor must occur exactly once",
    )?;
    for anchor in SETUP_ANCHORS {
        let replacement = anchor.replacen("        Self {", "        std::hint::black_box(calibration_layout_probe as fn(u64) -> u64);\n        std::hint::black_box(calibration_allocation_control as fn());\n        Self {", 1);
        text = exact_replace(&text, anchor, &replacement)?;
    }
    let (argument, body) = if variant == "layout" {
        let mut body = String::new();
        for index in 0..256_u64 {
            let constant = 0x9E37_79B9_7F4A_7C15_u64.wrapping_mul(index + 1);
            writeln!(
                body,
                "    state = state.rotate_left({}).wrapping_mul({}_u64) ^ {constant}_u64;",
                index % 63 + 1,
                constant | 1
            )?;
        }
        body.push_str("    state");
        ("mut state", body)
    } else {
        ("state", "    state".to_owned())
    };
    let helpers = format!(
        "#[inline(never)]\nfn {PROBE}({argument}: u64) -> u64 {{\n{body}\n}}\n\n{HELPERS}struct WarmSnapshot {{"
    );
    text = exact_replace(&text, "struct WarmSnapshot {", &helpers)?;
    let query = "calibration_inlay_query(\n        std::hint::black_box(fixture.snapshot),\n        std::hint::black_box(fixture.source.len()),\n    )";
    let measured = match variant {
        "extra_work" => {
            format!("    std::hint::black_box({query});\n    std::hint::black_box({query})")
        }
        "extra_alloc" => format!(
            "    let result = {query};\n    calibration_allocation_control();\n    std::hint::black_box(result)"
        ),
        _ => format!("    std::hint::black_box({query})"),
    };
    exact_replace(
        &text,
        INLAY_ANCHOR,
        &format!(
            "fn inlay_hints_warm(fixture: WarmSnapshot) -> Vec<analysis::InlayHint> {{\n{measured}\n}}"
        ),
    )
}

pub(super) fn balanced_order(job_id: &str, pair: u64) -> ToolResult<Vec<&'static str>> {
    require(pair > 0, "pair must be positive")?;
    let digest = Sha256::digest(job_id.as_bytes());
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let seed = u64::from_be_bytes(bytes);
    // Modular addition avoids wrapping at high SHA256 seeds.
    let offset = usize::try_from((seed % 5 + (pair - 1) % 5) % 5)?;
    let mut order: Vec<_> = (0..5).map(|index| VARIANTS[(index + offset) % 5]).collect();
    if !(seed / 5 % 2 + (pair - 1) / 5 % 2).is_multiple_of(2) {
        order.reverse();
    }
    Ok(order)
}

fn clean_environment() -> Environment {
    env::vars_os()
        .filter(|(key, _)| {
            !key.to_string_lossy().starts_with("IAI_CALLGRIND_") && key != "CARGO_TARGET_DIR"
        })
        .collect()
}

fn run_logged(
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

fn command_strings(command: &[&str]) -> Vec<OsString> {
    command.iter().map(OsString::from).collect()
}

fn environment_metadata(
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

fn host_metadata() -> ToolResult<Value> {
    let kernel = Command::new("uname").arg("-r").output()?;
    let hostname = Command::new("hostname").output()?;
    let mut host = json!({"kernel": String::from_utf8(kernel.stdout)?.trim(), "hostname": String::from_utf8(hostname.stdout)?.trim(), "cpu": "", "runner_identity": env::var("RUNNER_NAME").unwrap_or_else(|_| "unknown".to_owned())});
    if Path::new("/proc/cpuinfo").is_file() {
        host["cpuinfo"] = fs::read_to_string("/proc/cpuinfo")?.into();
    }
    Ok(host)
}

fn freeze_source(checkout: &Path, manifest: &Value) -> ToolResult<()> {
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

struct Build {
    checkout: PathBuf,
    executable: PathBuf,
    manifest: Value,
    identities: BTreeSet<super::Identity>,
    metadata: Value,
}

fn independent_build(
    variant: &str,
    source: &Path,
    work: &Path,
    output: &Path,
    environment: &Environment,
    original: &Value,
    harness: &str,
) -> ToolResult<Build> {
    let checkout = work.join(variant);
    fs::create_dir(&checkout)?;
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
        source_manifest(&checkout, &[])? == *original,
        "source changed while copying; discard this calibration job",
    )?;
    fs::write(checkout.join("benches/analysis.rs"), harness)?;
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
        "--no-run",
        "--message-format=json",
        "--target-dir",
    ]);
    command.push(target.as_os_str().to_owned());
    let built = run_logged(
        &command,
        &checkout,
        &output.join(format!("builds/{variant}.stdout")),
        &output.join(format!("builds/{variant}.stderr")),
        &build_environment,
    )?;
    let executable = parse_executable(&built, &checkout, &target)?;
    let mut nm = command_strings(&["nm", "-C", "-S", "--defined-only"]);
    nm.push(executable.as_os_str().to_owned());
    let symbols = run_logged(
        &nm,
        &checkout,
        &output.join(format!("builds/{variant}.nm.stdout")),
        &output.join(format!("builds/{variant}.nm.stderr")),
        environment,
    )?;
    let probe = parse_probe_symbols(&symbols, variant == "layout")?;
    let metadata = json!({"source_sha256": manifest["sha256"], "binary_sha256": sha256_file(&executable)?, "executable": executable, "layout_probe": probe});
    require(
        source_manifest(&checkout, &[])? == manifest,
        "source manifest changed during build",
    )?;
    let listed = run_logged(
        &[executable.as_os_str().to_owned(), "--list".into()],
        &checkout,
        &output.join(format!("builds/{variant}.list.stdout")),
        &output.join(format!("builds/{variant}.list.stderr")),
        environment,
    )?;
    let identities = parse_workload_list(&listed)?;
    Ok(Build {
        checkout,
        executable,
        manifest,
        identities,
        metadata,
    })
}

fn ensure_immutable(build: &Build) -> ToolResult<()> {
    require(
        source_manifest(&build.checkout, &[])? == build.manifest
            && sha256_file(&build.executable)? == build.metadata["binary_sha256"],
        "immutable source or compiled executable changed before/during measurement",
    )
}

fn measure(
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
    let home = raw.join("iai");
    let baseline = baseline_name(job_id, pair, variant);
    let stdout = raw.join("stdout.jsonl");
    let stderr = raw.join("stderr.log");
    let command = vec![
        build.executable.as_os_str().to_owned(),
        "--output-format=json".into(),
        "--save-summary=pretty-json".into(),
        format!("--save-baseline={baseline}").into(),
        "--callgrind-args=--cache-sim=yes".into(),
        format!("--home={}", home.display()).into(),
    ];
    let measured = run_logged(&command, &build.checkout, &stdout, &stderr, environment)?;
    let (workloads, summaries) =
        parse_measurement(&measured, &baseline, &build.executable, &build.identities)?;
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
            let path = integrity::resolve_path(Path::new(emitted))?;
            require(
                path.starts_with(&home) && path.is_file(),
                "Callgrind output escaped fresh measurement home or is missing",
            )?;
            integrity::check_probe_absent(&fs::read_to_string(path)?)?;
            profile_count += 1;
        }
    }
    let sample = json!({"pair": pair, "variant": variant, "order": order, "raw_stdout": stdout.strip_prefix(output)?, "raw_stderr": stderr.strip_prefix(output)?, "workloads": workloads, "verified_callgrind_profiles": profile_count});
    write_json(&raw.join("sample.json"), &sample)?;
    Ok(sample)
}

fn collect_job(
    args: &CollectArgs,
    source: &Path,
    work: &Path,
    output: &Path,
    dataset: &mut Value,
) -> ToolResult<()> {
    let environment = clean_environment();
    dataset["environment"] = environment_metadata(source, output, &environment)?;
    dataset["host"] = host_metadata()?;
    let revision = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["rev-parse", "HEAD"])
        .output()?;
    dataset["source_revision"] = if revision.status.success() {
        String::from_utf8(revision.stdout)?.trim().into()
    } else {
        "unknown".into()
    };
    let excluded = [work.to_owned(), output.to_owned()];
    let original = source_manifest(source, &excluded)?;
    write_json(&output.join("source-manifest.json"), &original)?;
    let harness_path = source.join("benches/analysis.rs");
    let harness = fs::read_to_string(&harness_path)?;
    dataset["harness_sha256"] = sha256_file(&harness_path)?.into();
    dataset["fixture_sha256"] =
        source_manifest(&source.join("benches/fixtures"), &excluded)?["sha256"].clone();
    let generated = VARIANTS
        .iter()
        .map(|variant| Ok((*variant, variant_harness(&harness, variant)?)))
        .collect::<ToolResult<BTreeMap<_, _>>>()?;
    let mut builds = BTreeMap::new();
    for variant in VARIANTS {
        let build = independent_build(
            variant,
            source,
            work,
            output,
            &environment,
            &original,
            &generated[variant],
        )?;
        if let Some(previous) = builds.values().next() {
            let previous: &Build = previous;
            require(
                build.identities == previous.identities,
                "independently built harness has a different executed workload manifest",
            )?;
        }
        dataset["variants"][variant] = build.metadata.clone();
        builds.insert(variant, build);
        write_json(&output.join("partial-data.json"), dataset)?;
    }
    require(
        dataset["variants"]["a"]["source_sha256"] == dataset["variants"]["b"]["source_sha256"],
        "A/A independent builds have different source manifests",
    )?;
    for pair in 1..=args.pairs {
        for (order, variant) in balanced_order(&args.job_id, pair)?.into_iter().enumerate() {
            let sample = measure(
                &builds[variant],
                &args.job_id,
                pair,
                variant,
                order,
                output,
                &environment,
            )?;
            dataset["samples"]
                .as_array_mut()
                .ok_or("samples must be an array")?
                .push(sample);
            write_json(&output.join("partial-data.json"), dataset)?;
        }
    }
    for variant in VARIANTS {
        ensure_immutable(&builds[variant])?;
        dataset["variants"][variant]["layout_probe_collected"] = false.into();
    }
    require(
        source_manifest(source, &excluded)? == original,
        "original source changed during calibration; job cannot be compared",
    )?;
    write_json(&output.join("data.json"), dataset)?;
    println!(
        "Collected {} complete pairs / {} fresh-process measurements in {}",
        args.pairs,
        dataset["samples"].as_array().map_or(0, Vec::len),
        output.join("data.json").display()
    );
    Ok(())
}

pub(super) fn collect(args: &CollectArgs) -> ToolResult<()> {
    let source = integrity::resolve_path(&args.source)?;
    let work = integrity::resolve_path(&args.work_dir)?;
    let output = integrity::resolve_path(&args.output_dir)?;
    integrity::validate_directories(&source, &work, &output)?;
    fs::create_dir_all(&work)?;
    fs::create_dir_all(&output)?;
    let mut dataset = json!({"format_version": 1, "job_id": args.job_id, "role": args.role, "variants": {}, "samples": []});
    if let Err(error) = collect_job(args, &source, &work, &output, &mut dataset) {
        write_json(&output.join("partial-data.json"), &dataset)?;
        write_json(
            &output.join("failure.json"),
            &json!({"error": error.to_string(), "type": "Error"}),
        )?;
        return Err(error);
    }
    Ok(())
}
