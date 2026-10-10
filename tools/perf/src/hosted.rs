use crate::{ToolResult, common, dashboard};
use clap::{Args, Parser};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const WORKFLOW: &str = "perf-diagnostics.yml";
const TARGETS: [&str; 6] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

#[derive(Args)]
struct HostedOptions {
    /// Repository containing the committed candidate and performance workflow.
    #[arg(long)]
    repo: Option<String>,
    /// Branch or tag containing the workflow; defaults to the repository's default branch.
    #[arg(long)]
    workflow_ref: Option<String>,
    #[arg(long, default_value = "all", value_parser = target)]
    target: String,
    /// A new directory for the downloaded evidence. Existing data is never replaced.
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Open the offline dashboard after downloading evidence.
    #[arg(long)]
    open: bool,
}

#[derive(Parser)]
#[command(no_binary_name = true)]
struct CompareArgs {
    #[arg(long, default_value = "main")]
    base: String,
    #[arg(long, default_value = "HEAD")]
    candidate: String,
    #[command(flatten)]
    hosted: HostedOptions,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Backend {
    Cpu,
    Heap,
    Native,
    Full,
}

#[derive(Parser)]
#[command(no_binary_name = true, group(clap::ArgGroup::new("backend").required(true).args(["cpu", "heap", "native", "full"])))]
struct ProfileArgs {
    #[arg(value_parser = scenario)]
    scenario: String,
    /// Sample CPU with samply in a separate LSP process.
    #[arg(long, num_args = 0, default_missing_value = "cpu")]
    cpu: Option<Backend>,
    /// Collect a symbolized Rust DHAT allocation profile.
    #[arg(long, num_args = 0, default_missing_value = "heap")]
    heap: Option<Backend>,
    /// Use perf, Instruments, or Windows Performance Recorder.
    #[arg(long, num_args = 0, default_missing_value = "native")]
    native: Option<Backend>,
    /// Collect paired measurements, CPU, heap, and native diagnostic profiles.
    #[arg(long, num_args = 0, default_missing_value = "full")]
    full: Option<Backend>,
    #[arg(long, value_parser = ["cpu", "heap"], requires = "native")]
    native_kind: Option<String>,
    #[arg(long, default_value = "HEAD")]
    candidate: String,
    #[arg(long, default_value = "main")]
    base: String,
    #[command(flatten)]
    hosted: HostedOptions,
}

struct Request {
    id: String,
    repository: String,
    workflow_ref: String,
    base_sha: String,
    candidate_sha: String,
    mode: String,
    scenario: String,
    native_kind: String,
    hosted: HostedOptions,
}

fn target(value: &str) -> Result<String, String> {
    if value == "all" || TARGETS.contains(&value) {
        Ok(value.to_owned())
    } else {
        Err(format!("Unsupported native target: {value}"))
    }
}

fn scenario(value: &str) -> Result<String, String> {
    if matches!(
        value,
        "discovery-10" | "discovery-1000" | "discovery-10000" | "latency"
    ) {
        Ok(value.to_owned())
    } else {
        Err(format!("Unknown scenario: {value}"))
    }
}

fn output(command: &mut Command) -> ToolResult<String> {
    let result = command.output()?;
    if !result.status.success() {
        return Err(format!(
            "Command failed ({}): {}",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn gh(args: &[&str]) -> ToolResult<String> {
    output(Command::new("gh").args(args))
}

fn repository(configured: Option<&str>) -> ToolResult<String> {
    let name = if let Some(name) = configured {
        name.to_owned()
    } else {
        gh(&[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ])?
    };
    let Some((owner, repository_name)) = name.split_once('/') else {
        return Err("Repository must be an owner/name pair".into());
    };
    if [owner, repository_name].iter().any(|part| {
        part.is_empty()
            || !part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            || matches!(*part, "." | "..")
    }) {
        return Err("Repository must be an owner/name pair".into());
    }
    Ok(name)
}

fn exact_sha(repository: &str, revision: &str) -> ToolResult<String> {
    let revision = if revision == "HEAD" {
        output(Command::new("git").args(["rev-parse", "--verify", "HEAD"]))?
    } else {
        revision.to_owned()
    };
    let mut endpoint = url::Url::parse("https://api.github.com/repos/")?;
    endpoint
        .path_segments_mut()
        .map_err(|()| "Invalid GitHub API endpoint")?
        .pop_if_empty()
        .extend(repository.split('/'))
        .push("commits")
        .push(&revision);
    let sha = gh(&["api", endpoint.path(), "--jq", ".sha"]).map_err(|error| {
        format!("Revision {revision:?} must be pushed to {repository}: {error}")
    })?;
    validate_sha(&sha)?;
    Ok(sha)
}

fn validate_sha(sha: &str) -> ToolResult<()> {
    if sha.len() != 40
        || !sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("Expected an exact lowercase 40-character source SHA".into());
    }
    Ok(())
}

fn workflow_ref(repository: &str, configured: Option<&str>) -> ToolResult<String> {
    if let Some(reference) = configured {
        if reference.is_empty() || reference.starts_with('-') {
            return Err("Workflow reference must be a non-empty branch or tag".into());
        }
        return Ok(reference.to_owned());
    }
    gh(&[
        "repo",
        "view",
        repository,
        "--json",
        "defaultBranchRef",
        "--jq",
        ".defaultBranchRef.name",
    ])
}

fn request(base: &str, candidate: &str, hosted: HostedOptions) -> ToolResult<Request> {
    gh(&["auth", "status"])?;
    let repository = repository(hosted.repo.as_deref())?;
    let workflow_ref = workflow_ref(&repository, hosted.workflow_ref.as_deref())?;
    let base_sha = exact_sha(&repository, base)?;
    let candidate_sha = exact_sha(&repository, candidate)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    Ok(Request {
        id: format!("perf-{}-{stamp}", std::process::id()),
        repository,
        workflow_ref,
        base_sha,
        candidate_sha,
        mode: String::new(),
        scenario: String::new(),
        native_kind: "cpu".to_owned(),
        hosted,
    })
}

pub(crate) fn run(command: &str, args: &[String]) -> ToolResult<()> {
    let request = match command {
        "compare" => {
            let args = CompareArgs::try_parse_from(args)?;
            let mut request = request(&args.base, &args.candidate, args.hosted)?;
            "compare".clone_into(&mut request.mode);
            "discovery-10000".clone_into(&mut request.scenario);
            request
        }
        "profile" => {
            let args = ProfileArgs::try_parse_from(args)?;
            if args.native_kind.is_some() && args.native.is_none() {
                return Err(clap::Error::raw(
                    clap::error::ErrorKind::ArgumentConflict,
                    "--native-kind applies only to --native",
                )
                .into());
            }
            let mut request = request(&args.base, &args.candidate, args.hosted)?;
            match args.cpu.or(args.heap).or(args.native).or(args.full) {
                Some(Backend::Cpu) => "cpu",
                Some(Backend::Heap) => "heap",
                Some(Backend::Native) => "native",
                Some(Backend::Full) => "full",
                None => return Err("A profiling backend is required".into()),
            }
            .clone_into(&mut request.mode);
            request.scenario = args.scenario;
            if let Some(kind) = args.native_kind {
                request.native_kind = kind;
            }
            request
        }
        _ => return Err(format!("Unsupported hosted command: {command}").into()),
    };
    dispatch(&request)
}

fn dispatch(request: &Request) -> ToolResult<()> {
    let mut command = Command::new("gh");
    command.args([
        "workflow",
        "run",
        WORKFLOW,
        "--repo",
        &request.repository,
        "--ref",
        &request.workflow_ref,
    ]);
    for (name, value) in [
        ("request_id", request.id.as_str()),
        ("mode", request.mode.as_str()),
        ("base_sha", request.base_sha.as_str()),
        ("candidate_sha", request.candidate_sha.as_str()),
        ("scenario", request.scenario.as_str()),
        ("target", request.hosted.target.as_str()),
        ("native_kind", request.native_kind.as_str()),
    ] {
        command.arg("-f").arg(format!("{name}={value}"));
    }
    output(&mut command).map_err(|error| {
        format!("Cannot dispatch {WORKFLOW} at {}. The workflow must exist on the default branch and the selected ref: {error}", request.workflow_ref)
    })?;
    println!(
        "Hosted request {}: {} → {}",
        request.id, request.base_sha, request.candidate_sha
    );
    let run = await_run(request)?;
    let status = Command::new("gh")
        .args([
            "run",
            "watch",
            &run.to_string(),
            "--repo",
            &request.repository,
            "--exit-status",
            "--interval",
            "30",
        ])
        .status()?;
    let root = new_output(
        request.hosted.output_dir.as_deref(),
        &request.repository,
        run,
    )?;
    let expected_targets: Vec<&str> = if request.hosted.target == "all" {
        TARGETS.to_vec()
    } else {
        vec![&request.hosted.target]
    };
    common::write_json(
        &root.join("run-manifest.json"),
        &json!({
            "format_version": 1, "run_id": run, "request_id": request.id,
            "repository": request.repository, "mode": request.mode,
            "base_sha": request.base_sha, "candidate_sha": request.candidate_sha,
            "scenario": request.scenario, "expected_targets": expected_targets,
            "workflow_succeeded": status.success(),
        }),
    )?;
    download(
        &request.repository,
        run,
        &format!("perf-{}-*", request.id),
        &root,
    )?;
    retain_workflow_run(&root, &request.repository, run)?;
    render(&root, request.hosted.open)?;
    if !status.success() {
        return Err(format!(
            "Hosted collection failed; preserved evidence is at {}",
            root.display()
        )
        .into());
    }
    Ok(())
}

fn matching_run(rows: &Value, title: &str, branch: &str) -> ToolResult<Option<u64>> {
    let rows = rows.as_array().ok_or("Malformed workflow-run list")?;
    let mut result = None;
    for row in rows {
        if row["displayTitle"] != title
            || row["headBranch"] != branch
            || row["event"] != "workflow_dispatch"
        {
            continue;
        }
        let id = row["databaseId"]
            .as_u64()
            .ok_or("Malformed workflow-run ID")?;
        if result.replace(id).is_some() {
            return Err(
                "Ambiguous workflow runs for the unique request ID; refusing to select one".into(),
            );
        }
    }
    Ok(result)
}

fn await_run(request: &Request) -> ToolResult<u64> {
    let started = Instant::now();
    let title = format!("perf-diagnostics / {}", request.id);
    loop {
        let rows = gh(&[
            "run",
            "list",
            "--repo",
            &request.repository,
            "--workflow",
            WORKFLOW,
            "--event",
            "workflow_dispatch",
            "--limit",
            "100",
            "--json",
            "databaseId,displayTitle,headBranch,event",
        ])?;
        if let Some(run) =
            matching_run(&serde_json::from_str(&rows)?, &title, &request.workflow_ref)?
        {
            return Ok(run);
        }
        if started.elapsed() >= Duration::from_secs(180) {
            return Err(format!("Dispatched request {} did not appear. No other run was selected; inspect {}/actions.", request.id, request.repository).into());
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn new_output(configured: Option<&Path>, repository: &str, run: u64) -> ToolResult<PathBuf> {
    let path = configured.map_or_else(
        || {
            std::env::temp_dir()
                .join("bend2-perf-runs")
                .join(repository.replace('/', "-"))
                .join(run.to_string())
        },
        Path::to_path_buf,
    );
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    fs::create_dir(&path).map_err(|error| {
        format!(
            "Evidence output must be a new directory ({}): {error}",
            path.display()
        )
    })?;
    Ok(path)
}

fn download(repository: &str, run: u64, pattern: &str, root: &Path) -> ToolResult<()> {
    output(
        Command::new("gh")
            .args([
                "run",
                "download",
                &run.to_string(),
                "--repo",
                repository,
                "--pattern",
                pattern,
                "--pattern",
                &format!("perf-request-{run}-*"),
                "--pattern",
                &format!("callgrind-{run}-*"),
                "--dir",
            ])
            .arg(root),
    )?;
    Ok(())
}

fn retain_workflow_run(root: &Path, repository: &str, run: u64) -> ToolResult<()> {
    let metadata = gh(&[
        "run",
        "view",
        &run.to_string(),
        "--repo",
        repository,
        "--json",
        "databaseId,attempt,headSha,event,status,conclusion,jobs,url,workflowName",
    ])
    .and_then(|data| Ok(serde_json::from_str::<Value>(&data)?));
    let value = match metadata {
        Ok(metadata) => json!({"format_version":1,"repository":repository,"run":metadata}),
        Err(error) => json!({"format_version":1,"repository":repository,"run_id":run,
            "status":"failed","error":error.to_string()}),
    };
    common::write_json(&root.join("workflow-run.json"), &value)
}

fn render(root: &Path, open: bool) -> ToolResult<()> {
    let result = if open {
        dashboard::open(root)
    } else {
        dashboard::generate(root)
    };
    let html = root.join("index.html");
    if html.is_file() {
        println!("Unified report: {}", html.display());
    }
    result
}

#[derive(Parser)]
#[command(no_binary_name = true)]
struct OpenArgs {
    /// GitHub Actions run ID or an already downloaded report directory.
    run: String,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    output_dir: Option<PathBuf>,
    /// Open the validated CPU trace in samply instead of the static dashboard.
    #[arg(long)]
    cpu: bool,
    /// Select one captured scenario; required when the run contains several.
    #[arg(long, value_parser = scenario, requires = "cpu")]
    scenario: Option<String>,
    /// Select the CPU profile's native target; defaults to this machine's target.
    #[arg(long, value_parser = target, requires = "cpu")]
    target: Option<String>,
}

pub(crate) fn open_args(args: &[String]) -> ToolResult<()> {
    let args = OpenArgs::try_parse_from(args)?;
    let path = Path::new(&args.run);
    if path.exists() {
        if args.repo.is_some() || args.output_dir.is_some() {
            return Err("--repo/--output-dir only apply when downloading a hosted run".into());
        }
        let root = if path.is_dir() {
            path
        } else if path.file_name().is_some_and(|name| name == "index.html") {
            path.parent().ok_or("Report has no parent directory")?
        } else {
            return Err("Open expects a downloaded report directory or its index.html".into());
        };
        return open_evidence(root, &args);
    }
    let run: u64 = args
        .run
        .parse()
        .map_err(|_| "Expected an existing report directory or numeric hosted run ID")?;
    if run == 0 {
        return Err("Hosted run ID must be positive".into());
    }
    let repository = repository(args.repo.as_deref())?;
    let root = new_output(args.output_dir.as_deref(), &repository, run)?;
    download(&repository, run, "perf-*", &root)?;
    retain_workflow_run(&root, &repository, run)?;
    open_evidence(&root, &args)
}

fn host_target() -> ToolResult<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        ("windows", "aarch64") => Ok("aarch64-pc-windows-msvc"),
        _ => Err("Select a supported CPU profile target explicitly with --target".into()),
    }
}

fn open_evidence(root: &Path, args: &OpenArgs) -> ToolResult<()> {
    if !args.cpu {
        return render(root, true);
    }
    let target = if let Some(target) = args.target.as_deref() {
        target
    } else {
        host_target()?
    };
    if target == "all" {
        return Err("A CPU viewer opens one native target; select its exact triple".into());
    }
    let (manifest, status) =
        dashboard::cpu_profile_manifest(root, target, args.scenario.as_deref())?;
    println!("Validated CPU profile: {}", manifest.display());
    crate::profiling::open_cpu_manifest(&manifest)?;
    if matches!(status.as_str(), "failed" | "incomplete" | "regression") {
        return Err(
            format!("Profile viewer closed; the preserved aggregate report is {status}").into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_identity_never_selects_latest_or_other_branch() -> ToolResult<()> {
        let rows = json!([
            {"databaseId": 80, "displayTitle":"perf-diagnostics / perf-current", "headBranch":"untrusted", "event":"workflow_dispatch"},
            {"databaseId": 79, "displayTitle":"perf-diagnostics / perf-other", "headBranch":"main", "event":"workflow_dispatch"},
            {"databaseId": 78, "displayTitle":"perf-diagnostics / perf-current", "headBranch":"main", "event":"pull_request"},
            {"databaseId": 77, "displayTitle":"perf-diagnostics / perf-current", "headBranch":"main", "event":"workflow_dispatch"}
        ]);
        assert_eq!(
            matching_run(&rows, "perf-diagnostics / perf-current", "main")?,
            Some(77)
        );
        assert_eq!(
            matching_run(&rows, "perf-diagnostics / absent", "main")?,
            None
        );
        let duplicate = json!([rows[3], rows[3]]);
        assert!(matching_run(&duplicate, "perf-diagnostics / perf-current", "main").is_err());
        Ok(())
    }

    #[test]
    fn native_kind_cannot_dispatch_a_cpu_collection() -> ToolResult<()> {
        let arguments = ["discovery-10000", "--cpu", "--native-kind", "heap"].map(str::to_owned);
        let error = run("profile", &arguments)
            .err()
            .ok_or("Invalid backend options dispatched a run")?;
        let error = error
            .downcast_ref::<clap::Error>()
            .ok_or("Invalid options reached hosted I/O")?;
        assert_eq!(error.exit_code(), 2);
        Ok(())
    }

    #[test]
    fn evidence_download_refuses_to_replace_existing_files() -> ToolResult<()> {
        let temporary = tempfile::tempdir()?;
        let output = temporary.path().join("evidence");
        fs::create_dir(&output)?;
        fs::write(output.join("sentinel"), b"original user data")?;
        assert!(new_output(Some(&output), "owner/repository", 42).is_err());
        assert_eq!(fs::read(output.join("sentinel"))?, b"original user data");
        Ok(())
    }
}
