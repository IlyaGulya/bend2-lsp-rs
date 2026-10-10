use crate::{
    ToolResult, common, doctor,
    scenario::{self, Scenario, ScenarioSession},
};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

mod sessions;
mod validation;
mod viewer;
mod windows;
use sessions::Session;

#[derive(Parser)]
#[command(group(clap::ArgGroup::new("backend").required(true).args(["cpu", "heap", "native"])))]
struct Arguments {
    scenario: String,
    #[arg(long)]
    cpu: bool,
    #[arg(long)]
    heap: bool,
    #[arg(long)]
    native: bool,
    #[arg(long, default_value = "cpu", requires = "native", value_parser = ["cpu", "heap"])]
    native_kind: String,
    #[arg(long)]
    binary: PathBuf,
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long)]
    candidate_revision: String,
}

fn files(directory: &Path, result: &mut Vec<PathBuf>) -> ToolResult<()> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_cached_key(fs::DirEntry::file_name);
    for entry in entries {
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(format!(
                "Profile artifacts must not contain symlinks: {}",
                entry.path().display()
            )
            .into());
        }
        if kind.is_dir() {
            files(&entry.path(), result)?;
        } else if kind.is_file() {
            result.push(entry.path());
        }
    }
    Ok(())
}

fn identity(path: &Path, kind: &str) -> ToolResult<Value> {
    Ok(json!({"path": path.canonicalize()?, "sha256": common::sha256_file(path)?, "kind": kind}))
}

fn binary_identity(binary: &Path, output: &Path) -> ToolResult<Value> {
    let directory = binary.parent().ok_or("Binary has no directory")?;
    let mut symbols = Vec::new();
    let stem = binary
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or("Non-Unicode binary name")?
        .replace('-', "_");
    if cfg!(windows) {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.extension().is_some_and(|extension| extension == "pdb")
                && path
                    .file_stem()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.replace('-', "_") == stem)
            {
                symbols.push(path);
            }
        }
        if symbols.is_empty() {
            return Err("Missing matching application PDB; build with CARGO_PROFILE_RELEASE_DEBUG=1 and STRIP=none".into());
        }
    } else if cfg!(target_os = "macos") {
        let dsym = directory.join(format!(
            "{}.dSYM",
            binary
                .file_name()
                .ok_or("Missing binary name")?
                .to_string_lossy()
        ));
        if !dsym.is_dir() {
            return Err("Missing application dSYM; build with DEBUG=1, STRIP=none and SPLIT_DEBUGINFO=packed".into());
        }
        files(&dsym, &mut symbols)?;
        if symbols.is_empty() {
            return Err("Application dSYM is empty".into());
        }
    } else {
        let sections = doctor::output(
            "readelf",
            &[
                "--sections",
                "--wide",
                binary.to_str().ok_or("Non-Unicode binary path")?,
            ],
        )?;
        if !sections.contains(".debug_info") {
            return Err(
                "Binary has no embedded DWARF debug information; use DEBUG=1 and STRIP=none".into(),
            );
        }
    }
    let mut symbol_identities = Vec::new();
    for path in symbols {
        let relative = path.strip_prefix(directory)?;
        let copy = output.join("symbols").join(relative);
        fs::create_dir_all(copy.parent().ok_or("Missing symbol directory")?)?;
        fs::copy(&path, &copy)?;
        let mut item = identity(&path, "debug-symbols")?;
        item["artifact_path"] = json!(portable_path(copy.strip_prefix(output)?));
        symbol_identities.push(item);
    }
    let mut result = identity(binary, "executable")?;
    result["symbols"] = json!(symbol_identities);
    result["symbol_storage"] = json!(if cfg!(target_os = "linux") {
        "embedded DWARF in executable"
    } else {
        "separate copied debug symbols"
    });
    // Preserve the exact executable alongside trace/symbols for offline symbolication.
    let copy = output
        .join("symbols")
        .join(binary.file_name().ok_or("Missing binary filename")?);
    fs::create_dir_all(copy.parent().ok_or("Missing binary artifact directory")?)?;
    fs::copy(binary, &copy)?;
    result["artifact_path"] = json!(portable_path(copy.strip_prefix(output)?));
    Ok(result)
}

fn artifact_kind(path: &Path) -> &'static str {
    match path.extension().and_then(std::ffi::OsStr::to_str) {
        Some("etl") => "etw-trace",
        Some("pdb") => "debug-symbols",
        Some("log") => "profiler-log",
        Some("xml") => "trace-validation",
        _ if path.file_name().is_some_and(|name| name == "samply.json") => "samply-profile",
        _ if path
            .file_name()
            .is_some_and(|name| name == "dhat-heap.json") =>
        {
            "dhat-profile"
        }
        _ if path.file_name().is_some_and(|name| name == "perf.data") => "perf-trace",
        _ if path
            .components()
            .any(|component| component.as_os_str().to_string_lossy().ends_with(".trace")) =>
        {
            "instruments-trace-member"
        }
        _ => "profile-evidence",
    }
}

fn portable_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn artifacts(output: &Path) -> ToolResult<Vec<Value>> {
    let mut paths = Vec::new();
    files(output, &mut paths)?;
    paths.into_iter().filter(|path| path.file_name().is_none_or(|name| name != "profile-manifest.json")).map(|path| {
        Ok(json!({"path": portable_path(path.strip_prefix(output)?), "sha256": common::sha256_file(&path)?, "kind": artifact_kind(&path), "bytes": path.metadata()?.len()}))
    }).collect()
}

fn profile_backend(args: &Arguments) -> ToolResult<(&'static str, &'static str)> {
    let mode = if args.cpu {
        "cpu"
    } else if args.heap {
        "heap"
    } else if args.native {
        "native"
    } else {
        return Err("Select one profile backend".into());
    };
    let backend = if args.cpu {
        "samply"
    } else if args.heap {
        "dhat"
    } else {
        match std::env::consts::OS {
            "linux" => "perf",
            "macos" => "xctrace",
            "windows" => "wpr",
            _ => return Err("Unsupported native profiler OS".into()),
        }
    };
    Ok((mode, backend))
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    common::require_ci()?;
    let args = Arguments::try_parse_from(
        std::iter::once("profile".to_owned()).chain(args.iter().cloned()),
    )?;
    let scenario = Scenario::parse(&args.scenario)?;
    if args.candidate_revision.len() != 40
        || !args
            .candidate_revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("--candidate-revision must be an exact lowercase 40-character SHA".into());
    }
    let target = common::native_target()?;
    fs::create_dir_all(&args.output_dir)?;
    let output = args.output_dir.canonicalize()?;
    let manifest_path = output.join("profile-manifest.json");
    if fs::read_dir(&output)?.next().is_some() {
        return Err("Profile output must be empty; choose a fresh output directory".into());
    }
    let (mode, backend) = profile_backend(&args)?;
    let mut manifest = json!({
        "format_version": 1, "status": "running", "backend": backend, "profile_kind": if args.heap {"heap"} else {&args.native_kind},
        "scenario": scenario.name(), "source_sha": args.candidate_revision, "target": target,
        "binary": null, "profiler": {"version": null, "commands": [], "viewer_command": []},
        "phases": [], "artifacts": [], "errors": [], "scenario_result": null, "heap_summary": null,
        "hosted_run": {"run_id": std::env::var("GITHUB_RUN_ID").ok(), "attempt": std::env::var("GITHUB_RUN_ATTEMPT").ok(), "job": std::env::var("GITHUB_JOB").ok()},
        "measurement": "Separate diagnostic process; profiler overhead is not clean latency evidence"
    });
    common::write_json(&manifest_path, &manifest)?;
    let mut session = Session::new(backend, &args.native_kind, &output);
    let outcome = (|| -> ToolResult<()> {
        scenario::install_cancellation_handler()?;
        scenario::check_cancelled()?;
        let source = doctor::output("git", &["rev-parse", "HEAD"])?;
        if source != args.candidate_revision {
            return Err("Candidate checkout differs from --candidate-revision".into());
        }
        let binary = args.binary.canonicalize()?;
        manifest["binary"] = binary_identity(&binary, &output)?;
        manifest["doctor"] = doctor::require_backend(mode, &args.native_kind, Some(&binary))?;
        session.prepare(&binary)?;
        common::write_json(&manifest_path, &manifest)?;
        let result = scenario::execute(&binary, scenario, &output.join("scenario"), &mut session)?;
        manifest["scenario_result"] = result;
        session.validate(&binary)?;
        Ok(())
    })();
    // execute owns its child; this independently reaps/cleans profiler processes and native sessions.
    let cleanup = session.abort();
    manifest["profiler"] = session.profiler();
    manifest["phases"] = json!(session.phases);
    manifest["target_pid"] = json!(session.pid);
    manifest["capture_scope"] = json!(if backend == "wpr" {
        "system ETW session; analyze exact target_pid"
    } else {
        "target process PID"
    });
    manifest["heap_summary"] = std::mem::take(&mut session.heap_summary);
    let mut errors = Vec::new();
    if let Err(error) = &outcome {
        errors.push(error.to_string());
    }
    if let Err(error) = &cleanup {
        errors.push(format!("Profiler cleanup: {error}"));
    }
    match artifacts(&output) {
        Ok(values) => manifest["artifacts"] = json!(values),
        Err(error) => errors.push(format!("Artifact identity: {error}")),
    }
    manifest["status"] = json!(if errors.is_empty() {
        "complete"
    } else {
        "failed"
    });
    manifest["errors"] = json!(errors);
    common::write_json(&manifest_path, &manifest)?;
    outcome?;
    cleanup?;
    if manifest["status"] != "complete" {
        return Err("Profile artifact validation failed; see profile-manifest.json".into());
    }
    println!("{}", manifest_path.display());
    Ok(())
}

pub(crate) fn open_cpu_manifest(path: &Path) -> ToolResult<()> {
    viewer::open(path)
}
