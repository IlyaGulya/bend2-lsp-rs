use crate::{
    ToolResult, common, latency,
    native::discovery,
    platform_memory::{self, MemoryObserver},
    scenario,
    transport::{LspProcess, REQUEST_TIMEOUT, file_uri},
};
use clap::Parser;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

const FILE_COUNT: usize = 10000;
const CYCLES: usize = 3;

#[derive(Parser)]
#[command(no_binary_name = true)]
struct Args {
    #[arg(long)]
    binary: PathBuf,
    #[arg(long)]
    output: PathBuf,
}

fn digest(value: &Value) -> ToolResult<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

fn source_identity() -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut identity = json!({
        "checkout": root,
        "scope": "runner checkout identity; does not attest the supplied binary was built from it",
    });
    for (field, arguments) in [
        ("revision", vec!["rev-parse", "HEAD"]),
        (
            "working_tree_status",
            vec!["status", "--porcelain=v1", "--untracked-files=normal"],
        ),
        ("tracked_diff_sha256", vec!["diff", "HEAD", "--binary"]),
    ] {
        identity[field] = match Command::new("git")
            .current_dir(&root)
            .args(arguments)
            .output()
        {
            Ok(output) if output.status.success() => {
                if field == "tracked_diff_sha256" {
                    json!(format!("{:x}", Sha256::digest(&output.stdout)))
                } else {
                    json!(String::from_utf8_lossy(&output.stdout).trim())
                }
            }
            Ok(output) => json!({"unavailable": String::from_utf8_lossy(&output.stderr).trim()}),
            Err(error) => json!({"unavailable": error.to_string()}),
        };
    }
    identity
}

struct Runner<'a> {
    client: &'a mut LspProcess,
    report: &'a mut Value,
    output: &'a Path,
    memory: Result<MemoryObserver, String>,
    started: Instant,
    cycle: usize,
}

impl Runner<'_> {
    fn phase(&mut self, phase: &str) -> ToolResult<()> {
        self.report["active_phase"] = json!(phase);
        self.report["active_cycle"] = json!(self.cycle + 1);
        common::write_json(self.output, self.report)?;
        scenario::check_cancelled()
    }

    fn request(&mut self, method: &str, params: Value) -> ToolResult<Value> {
        scenario::check_cancelled()?;
        let (response, elapsed_ns) = self.client.request(method, params, None)?;
        self.report["last_response"] = json!({"method": method, "result": response});
        let evidence = json!({
            "cycle": self.cycle + 1, "phase": self.report["active_phase"],
            "method": method, "elapsed_ns": elapsed_ns,
            "result_count": response.as_array().map(Vec::len),
            "result_sha256": digest(&response)?,
        });
        self.report["requests"]
            .as_array_mut()
            .ok_or("Missing stress request evidence")?
            .push(evidence);
        Ok(response)
    }

    fn observe(&mut self, name: &str, observation: Value) -> ToolResult<()> {
        self.report["cycles"][self.cycle]["observations"][name] = observation;
        self.report["last_response"] = Value::Null;
        common::write_json(self.output, self.report)
    }

    fn checkpoint(&mut self, name: &str) -> ToolResult<()> {
        let sample = match &mut self.memory {
            Ok(observer) => observer.sample().map_err(|error| error.to_string()),
            Err(error) => Err(error.clone()),
        };
        let sample = match sample {
            Ok(sample) => sample,
            Err(error) => json!({"rss_bytes": null, "error": error}),
        };
        self.report["rss_checkpoints"]
            .as_array_mut()
            .ok_or("Missing report-only RSS evidence")?
            .push(json!({
                "cycle": self.cycle + 1, "phase": name, "pid": self.client.pid(),
                "elapsed_ms": self.started.elapsed().as_millis(), "sample": sample,
            }));
        common::write_json(self.output, self.report)
    }

    fn symbols(&mut self, name: &str, expected: &[Value]) -> ToolResult<()> {
        self.phase(name)?;
        let response = self.request("workspace/symbol", json!({"query": ""}))?;
        let scope = discovery::require_subset(&response, expected, name, true, true)?;
        self.observe(name, scope)
    }

    // Poll only cold discovery: omissions are allowed while it is building, but
    // duplicates, unexpected identities and stale overlay symbols fail at once.
    fn discovery_barrier(&mut self, name: &str, expected: &[Value]) -> ToolResult<()> {
        self.phase(name)?;
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let response = self.request("workspace/symbol", json!({"query": ""}))?;
            let scope = discovery::require_subset(&response, expected, name, true, false)?;
            self.report["cycles"][self.cycle]["discovery_polls"]
                .as_array_mut()
                .ok_or("Missing discovery poll evidence")?
                .push(json!({"phase": name, "scope": scope}));
            if scope["complete"] == true {
                return self.observe(name, scope);
            }
            common::write_json(self.output, self.report)?;
            if Instant::now() >= deadline {
                return Err(format!(
                    "{name}: did not expose exact root contents within {REQUEST_TIMEOUT:?}"
                )
                .into());
            }
            // Back off between semantic probes; elapsed time never proves readiness.
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn references(&mut self, name: &str, uri: &str, expected: &[Value]) -> ToolResult<()> {
        self.phase(name)?;
        let mut params = latency::position(uri, 0, 7);
        params["context"] = json!({"includeDeclaration": true});
        let response = self.request("textDocument/references", params)?;
        let scope = discovery::require_subset(&response, expected, name, false, true)?;
        self.observe(name, scope)
    }

    fn navigation(&mut self, name: &str, entry: &str, common: &str, kind: &str) -> ToolResult<()> {
        self.phase(name)?;
        let params = latency::position(entry, 2, 12);
        let hover = self.request("textDocument/hover", params.clone())?;
        let signature = format!("def identity(value: {kind}) -> {kind}");
        let previous = if kind == "U32" { "U64" } else { "U32" };
        latency::require_hover(
            &hover,
            &signature,
            Some(&format!("def identity(value: {previous}) -> {previous}")),
        )?;
        self.observe(
            &format!("{name}_hover"),
            json!({"signature": signature, "result": hover}),
        )?;
        let definition = self.request("textDocument/definition", params)?;
        let expected = json!({"uri": common, "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 12}}});
        if definition != expected {
            return Err(format!(
                "{name}: stale/wrong definition: {definition}; expected {expected}"
            )
            .into());
        }
        self.observe(
            &format!("{name}_definition"),
            json!({"result": definition, "exact": true}),
        )
    }

    fn root(&mut self, workspace: &Path, added: bool) -> ToolResult<()> {
        let folder = json!({"uri": file_uri(workspace)?, "name": "lifecycle-stress"});
        let (added, removed) = if added {
            (vec![folder], vec![])
        } else {
            (vec![], vec![folder])
        };
        self.client.notify(
            "workspace/didChangeWorkspaceFolders",
            json!({"event": {"added": added, "removed": removed}}),
        )
    }

    fn watched(&mut self, uri: &str, kind: u8) -> ToolResult<()> {
        self.client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes": [{"uri": uri, "type": kind}]}),
        )
    }

    fn cycle(
        &mut self,
        workspace: &Path,
        files: &BTreeMap<String, String>,
        symbols: &[Value],
        references: &[Value],
    ) -> ToolResult<()> {
        let common = file_uri(&workspace.join("common.bend"))?;
        let entry = file_uri(&workspace.join("entry.bend"))?;
        let original = files.get("common.bend").ok_or("Missing common source")?;
        self.phase("open")?;
        for (uri, source) in [
            (
                &entry,
                files.get("entry.bend").ok_or("Missing entry source")?,
            ),
            (&common, original),
        ] {
            self.client.notify(
                "textDocument/didOpen",
                latency::open_message(uri, source, 1)["params"].clone(),
            )?;
            self.client.wait_diagnostics(uri, Some(1))?;
        }
        self.navigation("opened", &entry, &common, "U32")?;
        self.symbols("opened_symbols", symbols)?;
        self.references("opened_references", &common, references)?;

        self.phase("edit")?;
        let marker = format!("lifecycle_cycle_{}", self.cycle + 1);
        let edited = format!("{}def {marker}: U32\n  1\n", original.replace("U32", "U64"));
        self.client.notify(
            "textDocument/didChange",
            latency::change_message(&common, &edited, 2)["params"].clone(),
        )?;
        self.client.wait_diagnostics(&common, Some(2))?;
        let mut edited_symbols = symbols.to_vec();
        edited_symbols.push(json!({"name": marker, "kind": 12, "location": {"uri": common, "range": {"start": {"line": 2, "character": 4}, "end": {"line": 2, "character": 4 + marker.len()}}}}));
        self.navigation("edited", &entry, &common, "U64")?;
        self.symbols("edited_symbols", &edited_symbols)?;
        self.references("edited_references", &common, references)?;
        self.checkpoint("edited")?;
        self.delete_reindex(
            workspace,
            files,
            &edited_symbols,
            references,
            &entry,
            &common,
        )?;
        self.close_and_replace_root(workspace, symbols, references)?;
        self.report["cycles"][self.cycle]["status"] = json!("complete");
        common::write_json(self.output, self.report)
    }

    fn delete_reindex(
        &mut self,
        workspace: &Path,
        files: &BTreeMap<String, String>,
        symbols: &[Value],
        references: &[Value],
        entry: &str,
        common: &str,
    ) -> ToolResult<()> {
        self.phase("delete")?;
        let deleted_name = "consumer_0000.bend";
        let deleted_path = workspace.join(deleted_name);
        let deleted_uri = file_uri(&deleted_path)?;
        fs::remove_file(&deleted_path)?;
        self.watched(&deleted_uri, 3)?;
        let surviving_symbols: Vec<_> = symbols
            .iter()
            .filter(|item| item["location"]["uri"] != deleted_uri)
            .cloned()
            .collect();
        let surviving_references: Vec<_> = references
            .iter()
            .filter(|item| item["uri"] != deleted_uri)
            .cloned()
            .collect();
        self.symbols("deleted_symbols", &surviving_symbols)?;
        self.references("deleted_references", common, &surviving_references)?;
        self.navigation("deleted", entry, common, "U64")?;

        self.phase("reindex")?;
        fs::write(
            &deleted_path,
            files.get(deleted_name).ok_or("Missing deleted source")?,
        )?;
        self.watched(&deleted_uri, 1)?;
        self.symbols("reindexed_symbols", symbols)?;
        self.references("reindexed_references", common, references)?;
        self.navigation("reindexed", entry, common, "U64")
    }

    fn close_and_replace_root(
        &mut self,
        workspace: &Path,
        symbols: &[Value],
        references: &[Value],
    ) -> ToolResult<()> {
        let common = file_uri(&workspace.join("common.bend"))?;
        let entry = file_uri(&workspace.join("entry.bend"))?;
        self.phase("close_dependency")?;
        self.client.close_document(&common)?;
        self.navigation("closed_dependency", &entry, &common, "U32")?;
        self.phase("close_entry")?;
        self.client.close_document(&entry)?;
        self.symbols("closed_symbols", symbols)?;
        self.references("closed_references", &common, references)?;
        self.checkpoint("closed")?;

        self.phase("remove_root")?;
        self.root(workspace, false)?;
        self.symbols("removed_root_symbols", &[])?;
        self.checkpoint("root_removed")?;
        self.phase("rapid_root_replacement")?;
        // No intervening queries/sleeps: each remove supersedes the just-started
        // discovery. This exercises cancellation without asserting it was observed.
        for _ in 0..2 {
            self.root(workspace, true)?;
            self.root(workspace, false)?;
        }
        self.symbols("rapid_removed_symbols", &[])?;
        self.phase("readd_root")?;
        self.root(workspace, true)?;
        self.discovery_barrier("readded_root_symbols", symbols)?;
        self.references("readded_root_references", &common, references)?;
        self.checkpoint("root_readded")
    }
}

fn execute(
    args: &Args,
    report: &mut Value,
    directory: &mut Option<tempfile::TempDir>,
    client: &mut Option<LspProcess>,
    started: Instant,
) -> ToolResult<()> {
    scenario::install_cancellation_handler()?;
    scenario::check_cancelled()?;
    let binary = args.binary.canonicalize()?;
    report["binary"] = json!({"path": binary, "sha256": common::sha256_file(&binary)?});
    let parent = args
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    *directory = Some(
        tempfile::Builder::new()
            .prefix("lifecycle-stress-fixture-")
            .tempdir_in(parent)?,
    );
    let workspace = directory
        .as_ref()
        .ok_or("Missing fixture directory")?
        .path()
        .canonicalize()?;
    report["fixture_path"] = json!(workspace);
    let (files, manifest) = discovery::dataset(FILE_COUNT);
    report["dataset_manifest_sha256"] = json!(digest(&manifest)?);
    report["dataset_hash_definition"] = json!(
        "SHA-256 of compact JSON for the sorted native discovery dataset manifest (filename, byte length, source SHA-256)"
    );
    report["dataset_manifest"] = manifest;
    for (name, source) in &files {
        scenario::check_cancelled()?;
        fs::write(workspace.join(name), source)?;
    }
    let (symbols, references) = discovery::expected_results(&workspace, &files)?;
    *client = Some(LspProcess::spawn(&binary, &workspace, &[])?);
    let child = client.as_mut().ok_or("Missing LSP child")?;
    child.defer_abort_to_owner();
    report["pid"] = json!(child.pid());
    let mut runner = Runner {
        memory: MemoryObserver::new(child.pid()).map_err(|error| error.to_string()),
        client: child,
        report,
        output: &args.output,
        started,
        cycle: 0,
    };
    for cycle in 1..=CYCLES {
        runner.report["cycles"].as_array_mut().ok_or("Missing cycle evidence")?.push(json!({
            "cycle": cycle, "pid": runner.client.pid(), "status": "pending", "observations": {}, "discovery_polls": [],
        }));
    }
    runner.phase("initialize")?;
    runner.client.initialize()?;
    runner.discovery_barrier("initial_root_symbols", &symbols)?;
    runner.references(
        "initial_root_references",
        &file_uri(&workspace.join("common.bend"))?,
        &references,
    )?;
    runner.checkpoint("initial_discovery")?;
    for cycle in 0..CYCLES {
        runner.cycle = cycle;
        runner.report["cycles"][cycle]["status"] = json!("running");
        runner.cycle(&workspace, &files, &symbols, &references)?;
    }
    runner.phase("shutdown")?;
    runner.client.finish()
}

pub(crate) fn run(arguments: &[String]) -> ToolResult<()> {
    let args = Args::try_parse_from(arguments)?;
    let mut report = json!({
        "format_version": 1, "scenario": "lifecycle-stress", "status": "running",
        "dataset_file_count": FILE_COUNT, "required_cycles": CYCLES,
        "request_timeout_ms": REQUEST_TIMEOUT.as_millis(),
        "source_identity": source_identity(), "binary": {"requested_path": args.binary},
        "cycles": [], "requests": [], "rss_checkpoints": [], "last_response": null,
        "limitations": [
            "Correctness stress, not a hosted performance measurement; no latency/RSS admission thresholds",
            "RSS checkpoints are report-only and do not prove leaks or allocation liveness",
            "Snapshot Arc/Weak liveness is tested separately in the direct WorkspaceDb lifecycle test",
            "Exercises only existing indexed symbols, import navigation, references and lifecycle behavior",
            "Rapid workspace-folder notifications exercise superseded discovery; no production cancellation telemetry",
        ],
        "memory_definitions": platform_memory::metadata(),
    });
    // Establish the output before preflight so missing binaries and setup errors
    // also leave a structured failure, not merely an error on stderr.
    common::write_json(&args.output, &report)?;
    let started = Instant::now();
    let mut directory = None;
    let mut client = None;
    let outcome = execute(&args, &mut report, &mut directory, &mut client, started);
    report["elapsed_ms"] = json!(started.elapsed().as_millis());
    if let Err(error) = &outcome {
        report["status"] = json!("failed");
        report["error"] = json!(error.to_string());
        if let Some(child) = &mut client {
            child.abort();
        }
        if let Some(fixture) = directory.take() {
            report["fixture_path"] = json!(fixture.keep());
            report["fixture_retained"] = json!(true);
        }
    } else {
        report["status"] = json!("complete");
        report["fixture_retained"] = json!(false);
        report["last_response"] = Value::Null;
    }
    if let Some(child) = &mut client {
        report["shutdown"] = serde_json::to_value(child.shutdown_evidence())?;
        report["stderr_tail"] = json!(
            child
                .stderr_tail()
                .unwrap_or_else(|error| format!("stderr unavailable: {error}"))
        );
    }
    common::write_json(&args.output, &report)?;
    outcome
}
