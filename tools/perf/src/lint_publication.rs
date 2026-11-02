use crate::{
    ToolResult, common, latency, scenario,
    transport::{LspProcess, REQUEST_TIMEOUT, file_uri},
};
use clap::Parser;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

const ENTRY_COUNT: usize = 32;
const TARGET_COUNT: usize = 128;
const SAMPLES: usize = 5;
// One batch queues 32 checks; its bound is separate from the unchanged RPC timeout.
const BATCH_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Parser)]
#[command(no_binary_name = true)]
struct Args {
    #[arg(long)]
    binary: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    source_sha: String,
    #[arg(long)]
    verify_only: bool,
}

struct Dataset {
    entries: Vec<(String, String)>,
    targets: Vec<PathBuf>,
    digest: String,
}

fn dataset(workspace: &Path) -> ToolResult<Dataset> {
    fs::create_dir(workspace)?;
    fs::write(workspace.join("bend-lint.json"), "{\"rules\":{}}")?;
    let mut hasher = Sha256::new();
    let mut imports = String::new();
    let mut targets = Vec::with_capacity(TARGET_COUNT);
    for index in 0..TARGET_COUNT {
        let name = format!("target{index:03}.bend");
        let source = format!("def value{index}: U32\n  0\n");
        fs::write(workspace.join(&name), &source)?;
        hasher.update(name.as_bytes());
        hasher.update(source.as_bytes());
        writeln!(imports, "import ./{name} as target{index:03}")?;
        targets.push(workspace.join(name));
    }
    let mut entries = Vec::with_capacity(ENTRY_COUNT);
    for index in 0..ENTRY_COUNT {
        let name = format!("entry{index:03}.bend");
        let source = format!("{imports}\ndef entry{index}: U32\n  0\n");
        let path = workspace.join(&name);
        fs::write(&path, &source)?;
        hasher.update(name.as_bytes());
        hasher.update(source.as_bytes());
        entries.push((file_uri(&path)?, source));
    }
    Ok(Dataset {
        entries,
        targets,
        digest: format!("{:x}", hasher.finalize()),
    })
}

fn analyzer(root: &Path, control: &Path) -> ToolResult<PathBuf> {
    fs::create_dir(control)?;
    fs::write(control.join("exit-code"), "0")?;
    let source = root.join("tests/fixtures/bend_lint_process.rs");
    let executable = control.join(format!(
        "publication-fixture{}",
        std::env::consts::EXE_SUFFIX
    ));
    let output = Command::new("rustc")
        .args(["--edition=2024", "-Dwarnings"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Cannot compile controlled analyzer: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(executable)
}

fn response(control: &Path, targets: &[PathBuf], code: &str) -> ToolResult<()> {
    let findings: Vec<_> = targets.iter().map(|path| json!({
        "code":code,"severity":"warning","message":"Controlled publication workload",
        "path":path,"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":9}},
        "fixes":[]
    })).collect();
    common::write_json(
        &control.join("response.json"),
        &json!({
            "ok":true,"findings":findings,"suppressed":[],"skipped":[]
        }),
    )
}

fn current_publications(client: &LspProcess, targets: &[String], code: &str) -> usize {
    targets
        .iter()
        .filter(|uri| {
            client
                .diagnostics(uri)
                .and_then(|params| params["diagnostics"].as_array())
                .is_some_and(|items| {
                    let mut lint = items.iter().filter(|item| item["source"] == "bend-lint");
                    lint.next().is_some_and(|item| item["code"] == code) && lint.next().is_none()
                })
        })
        .count()
}

fn ready(
    client: &mut LspProcess,
    dataset: &Dataset,
    targets: &[String],
    code: &str,
) -> ToolResult<()> {
    let deadline = Instant::now() + BATCH_TIMEOUT;
    let mut pending: Vec<&str> = dataset
        .entries
        .iter()
        .map(|(uri, _)| uri.as_str())
        .collect();
    loop {
        scenario::check_cancelled()?;
        let mut entry = 0;
        while entry < pending.len() {
            let (status, _) = client.request(
                "workspace/executeCommand",
                json!({
                    "command":"bend2.lint.status","arguments":[{"uri":pending[entry]}]
                }),
                None,
            )?;
            if status["state"] == "skipped" || status["state"] == "disabled" {
                return Err(format!("Controlled lint entry cannot complete: {status}").into());
            }
            if status["state"] == "ready" {
                pending.swap_remove(entry);
            } else {
                entry += 1;
            }
        }
        // Keep dispatching notifications after the last entry becomes ready.
        if pending.is_empty() {
            client.request(
                "workspace/executeCommand",
                json!({
                    "command":"bend2.lint.status","arguments":[{"uri":dataset.entries[0].0}]
                }),
                None,
            )?;
        }
        let completed = ENTRY_COUNT - pending.len();
        let publications = current_publications(client, targets, code);
        if completed == ENTRY_COUNT && publications == TARGET_COUNT {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("Publication batch incomplete: {completed}/{ENTRY_COUNT} entries ready, {publications}/{TARGET_COUNT} exact current findings").into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn collect(binary: &Path, root: &Path, report: &mut Value, measure: bool) -> ToolResult<()> {
    let directory = tempfile::Builder::new()
        .prefix("bend-lint-publication-")
        .tempdir()?;
    let workspace_parent = directory.path().canonicalize()?;
    let data = dataset(&workspace_parent.join("project"))?;
    let workspace = workspace_parent.join("project");
    let control = workspace_parent.join("control");
    let fixture = analyzer(root, &control)?;
    report["dataset_sha256"] = json!(data.digest);
    report["fixture_source_sha256"] = json!(common::sha256_file(
        &root.join("tests/fixtures/bend_lint_process.rs")
    )?);
    report["fixture_binary_sha256"] = json!(common::sha256_file(&fixture)?);
    let environment = [(
        OsString::from("BEND_LINT_FIXTURE_CONTROL"),
        control.clone().into_os_string(),
    )];
    let mut client = LspProcess::spawn(binary, &workspace, &environment)?;
    scenario::bind_cancellation(&mut client);
    client.initialize()?;
    for (index, (uri, source)) in data.entries.iter().enumerate() {
        let mut opened = latency::open_message(uri, source, 1);
        client.notify("textDocument/didOpen", opened["params"].take())?;
        let (symbols, _) = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
            None,
        )?;
        if symbols.as_array().is_none_or(|items| {
            !items
                .iter()
                .any(|symbol| symbol["name"] == format!("entry{index}"))
        }) {
            return Err("Publication entry failed its committed-source barrier".into());
        }
    }
    let targets = data
        .targets
        .iter()
        .map(|path| file_uri(path))
        .collect::<ToolResult<Vec<_>>>()?;
    let warmup_code = "fixture/publication/warmup";
    response(&control, &data.targets, warmup_code)?;
    let mut settings = client.settings().clone();
    settings["bend"] = json!({"lint":{"enabled":true,"path":fixture,"dangerousCodeActions":false,"timeoutMs":30000}});
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":settings}),
    )?;
    ready(&mut client, &data, &targets, warmup_code)?;
    for sample in 0..if measure { SAMPLES } else { 1 } {
        let code = format!("fixture/publication/{sample}");
        response(&control, &data.targets, &code)?;
        let started = measure.then(Instant::now);
        client.request(
            "workspace/executeCommand",
            json!({
                "command":"bend2.lint.run","arguments":[{"uri":data.entries[0].0}]
            }),
            None,
        )?;
        ready(&mut client, &data, &targets, &code)?;
        report["verified_batches"] = json!(sample + 1);
        if let Some(started) = started {
            report["samples_ns"]
                .as_array_mut()
                .ok_or("Missing publication samples")?
                .push(json!(u64::try_from(started.elapsed().as_nanos())?));
        }
    }
    client.finish()?;
    report["shutdown"] = serde_json::to_value(client.shutdown_evidence())?;
    report["analyzer_checks"] = json!(fs::read_to_string(control.join("calls"))?.lines().count());
    report["preparations"] = json!(
        fs::read_to_string(control.join("prepares"))?
            .lines()
            .count()
    );
    Ok(())
}

pub(crate) fn run(arguments: &[String]) -> ToolResult<()> {
    let args = Args::try_parse_from(arguments)?;
    if !args.verify_only {
        common::require_ci()?;
    }
    if args.source_sha.len() != 40
        || !args
            .source_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("--source-sha must be an exact lowercase commit SHA".into());
    }
    scenario::install_cancellation_handler()?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let source = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&root)
        .output()?;
    if !source.status.success() || String::from_utf8_lossy(&source.stdout).trim() != args.source_sha
    {
        return Err("Publication collection checkout does not match --source-sha".into());
    }
    let binary = args.binary.canonicalize()?;
    let mut report = json!({
        "format_version":1,"status":"running",
        "mode":if args.verify_only { "correctness-only" } else { "ci-observation" },
        "source_sha":(!args.verify_only).then_some(&args.source_sha),"checkout_sha":args.source_sha,
        "repository":std::env::var("GITHUB_REPOSITORY").ok(),
        "run_id":std::env::var("GITHUB_RUN_ID").ok(),
        "run_attempt":std::env::var("GITHUB_RUN_ATTEMPT").ok(),
        "target":common::native_target()?,"binary_sha256":common::sha256_file(&binary)?,
        "harness_sha256":common::sha256_file(&std::env::current_exe()?)?,
        "entry_count":ENTRY_COUNT,"diagnostic_target_count":TARGET_COUNT,
        "batch_timeout_ms":BATCH_TIMEOUT.as_millis(),"rpc_timeout_ms":REQUEST_TIMEOUT.as_millis(),
        "warmup_batches":1,"verified_batches":0,
        "samples_ns":if args.verify_only { Value::Null } else { json!([]) },
        "comparison":if args.verify_only { "not a measurement" } else { "unpaired baseline; no regression verdict" },
        "scope":"Full stdio manual recheck through current multi-entry diagnostics; includes snapshot/freshness, controlled analyzer subprocesses, publication and client readiness. Not isolated publisher CPU or real Bend analysis."
    });
    common::write_json(&args.output, &report)?;
    let outcome = collect(&binary, &root, &mut report, !args.verify_only);
    report["status"] = json!(if outcome.is_ok() {
        "complete"
    } else {
        "failed"
    });
    if let Err(error) = &outcome {
        report["error"] = json!(error.to_string());
    }
    common::write_json(&args.output, &report)?;
    outcome
}
