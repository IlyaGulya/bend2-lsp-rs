//! Collect independent functions without changing frozen benchmark children.
use super::{
    BENCHMARKS, Identity,
    collect::{Build, Environment, run_logged},
    descriptor::{self, CAPTURE_DIRECTORY},
    require,
    runner::{self, RunnerJob},
};
use crate::{
    ToolResult,
    common::{sha256_file, write_json},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

struct CaptureContext<'a> {
    build: &'a Build,
    baseline: &'a str,
    raw: &'a Path,
    environment: &'a Environment,
    shim: &'a Path,
    original_runner: &'a Path,
}

struct CapturedBenchmark {
    name: &'static str,
    directory: PathBuf,
    jobs: Vec<usize>,
    execution: Value,
}

fn resolve_runner(environment: &Environment, checkout: &Path) -> ToolResult<PathBuf> {
    let path = environment
        .get(OsStr::new("PATH"))
        .ok_or("missing runner PATH")?;
    for directory in env::split_paths(path) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            checkout.join(directory)
        };
        let candidate = directory.join("iai-callgrind-runner");
        if !candidate.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if fs::metadata(&candidate)?.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Ok(fs::canonicalize(candidate)?);
    }
    Err("original iai-callgrind-runner is absent from measurement PATH".into())
}

fn create_capture_runner(path: &Path, executable: &Path) -> ToolResult<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(executable, path)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (path, executable);
        Err("Callgrind descriptor collection requires Unix".into())
    }
}

fn capture(
    context: &CaptureContext<'_>,
    benchmark: &'static str,
    selected: &BTreeSet<Identity>,
    jobs: &mut Vec<RunnerJob>,
) -> ToolResult<CapturedBenchmark> {
    let directory = context.raw.join(benchmark);
    fs::create_dir(&directory)?;
    let home = directory.join("iai");
    let descriptor_directory = directory.join("descriptor");
    let compiled = &context.build.benchmarks[benchmark];
    let mut environment = context.environment.clone();
    let original_path = environment
        .get(OsStr::new("PATH"))
        .ok_or("missing runner PATH")?;
    let path = env::join_paths(
        std::iter::once(context.shim.to_owned()).chain(env::split_paths(original_path)),
    )?;
    environment.insert("PATH".into(), path);
    environment.insert(
        CAPTURE_DIRECTORY.into(),
        descriptor_directory.as_os_str().to_owned(),
    );
    let command = vec![
        compiled.executable.as_os_str().to_owned(),
        "--output-format=json".into(),
        "--save-summary=pretty-json".into(),
        format!("--save-baseline={}", context.baseline).into(),
        "--callgrind-args=--cache-sim=yes".into(),
        format!("--home={}", home.display()).into(),
    ];
    let output = run_logged(
        &command,
        &context.build.checkout,
        &directory.join("capture.stdout"),
        &directory.join("capture.stderr"),
        &environment,
    )?;
    require(
        output.trim().is_empty(),
        "descriptor capture unexpectedly produced measurement output",
    )?;
    let plan = descriptor::plan(
        &descriptor_directory,
        &compiled.identities,
        selected,
        &compiled.executable,
    )?;
    let mut indices = Vec::with_capacity(plan.functions.len());
    let mut manifests = Vec::with_capacity(plan.functions.len());
    for function in plan.functions {
        let job_directory = directory.join("functions").join(&function.label);
        fs::create_dir_all(&job_directory)?;
        fs::write(job_directory.join("descriptor.bin"), &function.bytes)?;
        let mut command = Vec::with_capacity(plan.runner_arguments.len() + 1);
        command.push(context.original_runner.as_os_str().to_owned());
        command.extend(plan.runner_arguments.iter().cloned());
        let length = command
            .last_mut()
            .ok_or("missing descriptor length argument")?;
        *length = OsString::from(function.bytes.len().to_string());
        indices.push(jobs.len());
        manifests.push(json!({"label":function.label,"directory":job_directory.strip_prefix(context.raw)?,"workloads":function.identities}));
        jobs.push(RunnerJob {
            command,
            input: function.bytes,
            stdout: job_directory.join("stdout.jsonl"),
            stderr: job_directory.join("stderr.log"),
        });
    }
    Ok(CapturedBenchmark {
        name: benchmark,
        directory,
        jobs: indices,
        execution: json!({"descriptor":descriptor_directory.strip_prefix(context.raw)?,"functions":manifests}),
    })
}

fn concatenate<'a>(destination: &Path, sources: impl Iterator<Item = &'a Path>) -> ToolResult<()> {
    let mut output = File::create(destination)?;
    for source in sources {
        io::copy(&mut File::open(source)?, &mut output)?;
    }
    Ok(())
}

pub(crate) fn measure(
    build: &Build,
    baseline: &str,
    raw: &Path,
    environment: &Environment,
    selected: &BTreeMap<&'static str, BTreeSet<Identity>>,
) -> ToolResult<BTreeMap<&'static str, Value>> {
    let tool = env::current_exe()?;
    let original_runner = resolve_runner(environment, &build.checkout)?;
    let runner_sha256 = sha256_file(&original_runner)?;
    // Runtime-only links must never enter immutable raw evidence snapshots.
    let shim = tempfile::Builder::new()
        .prefix("bend2-iai-descriptor-")
        .tempdir()?;
    create_capture_runner(&shim.path().join("iai-callgrind-runner"), &tool)?;
    let context = CaptureContext {
        build,
        baseline,
        raw,
        environment,
        shim: shim.path(),
        original_runner: &original_runner,
    };
    let mut jobs = Vec::new();
    let mut captures = Vec::with_capacity(BENCHMARKS.len());
    for benchmark in BENCHMARKS {
        if selected[benchmark].is_empty() {
            continue;
        }
        captures.push(capture(
            &context,
            benchmark,
            &selected[benchmark],
            &mut jobs,
        )?);
    }
    shim.close()?;
    let collection = json!({"parallelism":runner::MAX_PARALLELISM,"original_runner":original_runner,"original_runner_sha256":runner_sha256,"functions":jobs.len()});
    write_json(&raw.join("collection.json"), &collection)?;
    runner::run_jobs(&jobs, &build.checkout, environment, runner::MAX_PARALLELISM)?;
    require(
        sha256_file(&original_runner)? == runner_sha256,
        "original runner changed during collection",
    )?;
    let mut executions = BTreeMap::new();
    for mut captured in captures {
        concatenate(
            &captured.directory.join("stdout.jsonl"),
            captured
                .jobs
                .iter()
                .map(|index| jobs[*index].stdout.as_path()),
        )?;
        concatenate(
            &captured.directory.join("stderr.log"),
            captured
                .jobs
                .iter()
                .map(|index| jobs[*index].stderr.as_path()),
        )?;
        captured.execution["collection"] = json!("collection.json");
        executions.insert(captured.name, captured.execution);
    }
    Ok(executions)
}
