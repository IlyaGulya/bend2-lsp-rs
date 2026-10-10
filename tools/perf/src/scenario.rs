use crate::{
    ToolResult, common, latency,
    native::discovery,
    transport::{LspProcess, REQUEST_TIMEOUT, file_uri},
};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        LazyLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const SAMPLES: usize = 32;
const WARMUP: usize = 8;
const COMPLETION_URI: &str = "untitled:discovery-completion.bend";
const COMPLETION_SOURCE: &str = "def transform(value: U32) -> U32:\n  va\n";

static CANCELLED: AtomicBool = AtomicBool::new(false);
static CANCELLATION_HANDLER: LazyLock<Result<(), String>> = LazyLock::new(|| {
    ctrlc::set_handler(|| CANCELLED.store(true, Ordering::Relaxed))
        .map_err(|error| error.to_string())
});

pub(crate) fn install_cancellation_handler() -> ToolResult<()> {
    match &*CANCELLATION_HANDLER {
        Ok(()) => Ok(()),
        Err(error) => {
            Err(format!("Cannot install performance scenario cancellation handler: {error}").into())
        }
    }
}

/// Cooperative cancellation is sticky for the lifetime of this hosted run.
pub(crate) fn check_cancelled() -> ToolResult<()> {
    if CANCELLED.load(Ordering::Relaxed) {
        Err(
            "Performance scenario cancelled; stopping owned collectors and restoring native state"
                .into(),
        )
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scenario {
    Discovery10,
    Discovery1000,
    Discovery10000,
    Latency,
}

impl Scenario {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Discovery10 => "discovery-10",
            Self::Discovery1000 => "discovery-1000",
            Self::Discovery10000 => "discovery-10000",
            Self::Latency => "latency",
        }
    }

    pub(crate) fn parse(value: &str) -> ToolResult<Self> {
        value.parse()
    }

    fn file_count(self) -> usize {
        match self {
            Self::Discovery10 | Self::Latency => 10,
            Self::Discovery1000 => 1000,
            Self::Discovery10000 => 10000,
        }
    }
}

impl FromStr for Scenario {
    type Err = Box<dyn std::error::Error + Send + Sync>;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "discovery-10" => Ok(Self::Discovery10),
            "discovery-1000" => Ok(Self::Discovery1000),
            "discovery-10000" => Ok(Self::Discovery10000),
            "latency" => Ok(Self::Latency),
            _ => Err(format!("Unknown scenario {value:?}; expected discovery-10, discovery-1000, discovery-10000, or latency").into()),
        }
    }
}

/// A collector attached to the actual protocol child, never to the harness.
/// `started` runs before initialize. Attach collectors can stop while their
/// target is alive; in-process heap collectors finalize after target shutdown.
pub(crate) trait ScenarioSession {
    fn started(&mut self, pid: u32) -> ToolResult<()>;
    fn phase(&mut self, name: &str) -> ToolResult<()>;
    fn finished(&mut self) -> ToolResult<()>;
    fn abort(&mut self) -> ToolResult<()>;

    fn child_environment(&self) -> Vec<(OsString, OsString)> {
        Vec::new()
    }

    fn finish_before_shutdown(&self) -> bool {
        false
    }

    fn finalization_timeout(&self) -> Duration {
        REQUEST_TIMEOUT
    }
}

pub(crate) struct NoProfiler;

impl ScenarioSession for NoProfiler {
    fn started(&mut self, _pid: u32) -> ToolResult<()> {
        Ok(())
    }

    fn phase(&mut self, _name: &str) -> ToolResult<()> {
        Ok(())
    }

    fn finished(&mut self) -> ToolResult<()> {
        Ok(())
    }

    fn abort(&mut self) -> ToolResult<()> {
        Ok(())
    }
}

/// Own both subprocess lifetimes even when a hook fails or the runner unwinds.
struct OwnedSession<'a> {
    session: &'a mut dyn ScenarioSession,
    client: Option<LspProcess>,
    complete: bool,
}

impl OwnedSession<'_> {
    fn abort(&mut self) -> ToolResult<()> {
        // Stop attach collectors before removing the process they are tracing.
        // Always reap the LSP, including when stopping the collector fails.
        let result = self.session.abort();
        if let Some(client) = &mut self.client {
            client.abort();
        }
        result
    }
}

impl Drop for OwnedSession<'_> {
    fn drop(&mut self) {
        if !self.complete {
            let _ = self.abort();
        }
    }
}

fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn record_phase(result: &mut Value, started: Instant, name: &str) -> ToolResult<()> {
    result["phases"]
        .as_array_mut()
        .ok_or("Missing scenario phase array")?
        .push(json!({"name": name, "elapsed_ns": elapsed(started)}));
    Ok(())
}

struct Workload<'a> {
    client: &'a mut LspProcess,
    session: &'a mut dyn ScenarioSession,
    result: &'a mut Value,
    started: Instant,
}

impl Workload<'_> {
    fn phase(&mut self, name: &str) -> ToolResult<()> {
        record_phase(self.result, self.started, name)?;
        self.client.check_cancelled()?;
        self.session.phase(name)
    }

    fn transition<T>(
        &mut self,
        name: &str,
        action: impl FnOnce(&mut Self) -> ToolResult<T>,
    ) -> ToolResult<T> {
        self.phase(&format!("{name}.before"))?;
        let started = Instant::now();
        let outcome = action(self);
        self.result["timings_ns"][name] = json!(elapsed(started));
        let value = outcome?;
        self.phase(&format!("{name}.after"))?;
        Ok(value)
    }

    fn request(
        &mut self,
        name: &str,
        method: &str,
        params: Value,
        notification: Option<Value>,
    ) -> ToolResult<Value> {
        let (response, duration) = self.client.request(method, params, notification)?;
        self.result["requests"]
            .as_array_mut()
            .ok_or("Missing scenario requests array")?
            .push(json!({
                "name":name,"method":method,"elapsed_ns":duration,
                "result_count":response.as_array().map(Vec::len),
            }));
        Ok(response)
    }

    fn discovery_samples(&mut self) -> ToolResult<()> {
        let requests = self.result["requests"]
            .as_array()
            .ok_or("Missing scenario request evidence")?;
        let mut workloads = BTreeMap::new();
        for name in [
            "hover_warm",
            "definition_warm",
            "references_warm",
            "workspace_symbol_warm",
            "dependency_edit_to_correct_hover",
        ] {
            let durations = requests
                .iter()
                .filter(|request| request["name"] == name)
                .map(|request| {
                    request["elapsed_ns"]
                        .as_u64()
                        .ok_or("Missing request duration")
                })
                .collect::<Result<Vec<_>, _>>()?;
            if durations.len() != WARMUP + SAMPLES {
                return Err(
                    format!("Discovery workload {name} did not produce every sample").into(),
                );
            }
            workloads.insert(name, json!(&durations[WARMUP..]));
        }
        self.result["workloads_ns"] = json!(workloads);
        Ok(())
    }

    fn exact_symbols(&mut self, name: &str, expected: &[Value]) -> ToolResult<()> {
        let symbols = self.request(name, "workspace/symbol", json!({"query":""}), None)?;
        self.result["semantics"][name] =
            discovery::require_subset(&symbols, expected, name, true, true)?;
        Ok(())
    }

    fn exact_references(
        &mut self,
        name: &str,
        params: Value,
        expected: &[Value],
    ) -> ToolResult<()> {
        let references = self.request(name, "textDocument/references", params, None)?;
        self.result["semantics"][name] =
            discovery::require_subset(&references, expected, name, false, true)?;
        Ok(())
    }

    fn cold_discovery(
        &mut self,
        workspace: &Path,
        symbols: &[Value],
        references: &[Value],
    ) -> ToolResult<()> {
        self.transition("cold_discovery", |run| {
            let started = Instant::now();
            let deadline = started + REQUEST_TIMEOUT;
            run.result["discovery_polls"] = json!([]);
            loop {
                let response = run.request(
                    "cold_symbols_poll",
                    "workspace/symbol",
                    json!({"query":""}),
                    None,
                )?;
                let scope =
                    discovery::require_subset(&response, symbols, "cold symbols", true, false)?;
                run.result["discovery_polls"]
                    .as_array_mut()
                    .ok_or("Missing discovery polls")?
                    .push(json!({"elapsed_ns":elapsed(started),"scope":scope}));
                if scope["complete"] == true {
                    run.result["semantics"]["cold_symbols"] = scope;
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("Discovery did not expose the exact complete symbol set".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
            let common = file_uri(&workspace.join("common.bend"))?;
            let mut params = latency::position(&common, 0, 7);
            params["context"] = json!({"includeDeclaration":true});
            run.exact_references("cold_references", params, references)
        })
    }

    fn remove_root(&mut self, workspace: &Path, symbols: &[Value]) -> ToolResult<()> {
        self.transition("root_removal", |run| {
            let root = file_uri(workspace)?;
            run.client.notify(
                "workspace/didChangeWorkspaceFolders",
                json!({"event":{"added":[],"removed":[{"uri":root,"name":"discovery"}]}}),
            )?;
            let started = Instant::now();
            let deadline = started + REQUEST_TIMEOUT;
            run.result["root_removal_polls"] = json!([]);
            loop {
                let response = run.request(
                    "symbols_after_root_removal",
                    "workspace/symbol",
                    json!({"query":""}),
                    None,
                )?;
                let scope = discovery::require_subset(
                    &response,
                    symbols,
                    "root removal symbols",
                    true,
                    false,
                )?;
                run.result["root_removal_polls"]
                    .as_array_mut()
                    .ok_or("Missing root removal polls")?
                    .push(
                        json!({"elapsed_ns":elapsed(started),"observed_symbols":scope["observed"]}),
                    );
                if scope["observed"] == 0 {
                    run.result["semantics"]["root_removal"] =
                        json!({"empty":true,"observed_symbols":0});
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err("Root removal did not release workspace symbols".into());
                }
                thread::sleep(Duration::from_millis(50));
            }
        })
    }

    fn discovery(
        &mut self,
        workspace: &Path,
        files: &BTreeMap<String, String>,
        symbols: &[Value],
        references: &[Value],
    ) -> ToolResult<()> {
        self.transition("local_completion", |run| {
            let result = run.request(
                "initial_open_to_local_completion",
                "textDocument/completion",
                latency::position(COMPLETION_URI, 1, 4),
                Some(latency::open_message(COMPLETION_URI, COMPLETION_SOURCE, 1)),
            )?;
            run.result["semantics"]["initial_completion"] =
                discovery::require_local_completion(&result)?;
            run.client.close_document(COMPLETION_URI)
        })?;
        self.cold_discovery(workspace, symbols, references)?;
        let common = file_uri(&workspace.join("common.bend"))?;
        let entry = file_uri(&workspace.join("entry.bend"))?;
        let hover_params = latency::position(&entry, 2, 12);
        let mut refs_params = hover_params.clone();
        refs_params["context"] = json!({"includeDeclaration":true});
        self.transition("open_entry", |run| {
            let hover = run.request(
                "open_entry_to_correct_hover",
                "textDocument/hover",
                hover_params.clone(),
                Some(latency::open_message(
                    &entry,
                    files.get("entry.bend").ok_or("Missing entry source")?,
                    1,
                )),
            )?;
            latency::require_hover(&hover, "def identity(value: U32) -> U32", None)?;
            run.client.wait_diagnostics(&entry, Some(1))?;
            run.result["semantics"]["entry_hover"] = json!({"correct":true});
            run.exact_references("references_after_open", refs_params.clone(), references)?;
            run.exact_symbols("symbols_after_open", symbols)
        })?;
        self.transition("discovery_warm_queries", |run| {
            for index in 0..WARMUP + SAMPLES {
                let hover = run.request(
                    "hover_warm", "textDocument/hover", hover_params.clone(), None,
                )?;
                latency::require_hover(&hover, "def identity(value: U32) -> U32", None)?;
                let definition = run.request(
                    "definition_warm", "textDocument/definition", hover_params.clone(), None,
                )?;
                let expected = json!({"uri":common,"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":12}}});
                if definition != expected {
                    return Err(format!("Incorrect definition: {definition}; expected {expected}").into());
                }
                run.exact_references("references_warm", refs_params.clone(), references)?;
                run.exact_symbols("workspace_symbol_warm", symbols)?;
                run.result["semantics"]["discovery_warm_queries"] =
                    json!({"validated_iterations":index + 1,"warmup":WARMUP,"samples":SAMPLES});
            }
            Ok(())
        })?;
        self.transition("dependency_revisions", |run| {
            let original = files.get("common.bend").ok_or("Missing common source")?;
            run.client.notify(
                "textDocument/didOpen",
                latency::open_message(&common, original, 1)["params"].clone(),
            )?;
            run.client.wait_diagnostics(&common, Some(1))?;
            for index in 0..WARMUP + SAMPLES {
                let (kind, old_kind) = if index % 2 == 0 { ("U64", "U32") } else { ("U32", "U64") };
                let version = i64::try_from(index)? + 2;
                let response = run.request(
                    "dependency_edit_to_correct_hover",
                    "textDocument/hover",
                    hover_params.clone(),
                    Some(latency::change_message(&common, &original.replace("U32", kind), version)),
                )?;
                latency::require_hover(
                    &response,
                    &format!("def identity(value: {kind}) -> {kind}"),
                    Some(&format!("def identity(value: {old_kind}) -> {old_kind}")),
                )?;
                run.client.wait_diagnostics(&common, Some(version))?;
                run.result["semantics"]["dependency_revisions"] =
                    json!({"validated_revisions":index + 1,"last_version":version,"last_type":kind});
            }
            run.exact_references("final_references", refs_params, references)?;
            run.exact_symbols("final_symbols", symbols)
        })?;
        self.discovery_samples()?;
        self.transition("close_overlays", |run| {
            run.client.close_document(&common)?;
            run.client.close_document(&entry)?;
            run.exact_symbols("symbols_after_close", symbols)
        })?;
        self.remove_root(workspace, symbols)
    }

    fn latency(&mut self, workspace: &Path, body: &str) -> ToolResult<()> {
        let completion_source = latency::SMALL_SOURCE.replace("  add(1, 2)\n", "  ad\n");
        let documents = [
            ("small", latency::SMALL_SOURCE),
            ("completion", completion_source.as_str()),
            ("dep", latency::DEPENDENCY_SOURCE),
            ("importer", latency::IMPORTER_SOURCE),
        ];
        let mut uris = BTreeMap::new();
        self.transition("latency_open_documents", |run| {
            for (name, source) in documents {
                let path = workspace.join(format!("{name}.bend"));
                fs::write(&path, source)?;
                let uri = file_uri(&path)?;
                run.client.notify(
                    "textDocument/didOpen",
                    latency::open_message(&uri, source, 1)["params"].clone(),
                )?;
                run.client.wait_diagnostics(&uri, Some(1))?;
                uris.insert(name, uri);
            }
            Ok(())
        })?;
        let small = latency::position(&uris["small"], 3, 4);
        let definition = latency::position(&uris["importer"], 2, 8);
        let completion = latency::position(&uris["completion"], 3, 4);
        for (name, method, params) in [
            ("hover_warm", "textDocument/hover", small),
            ("definition_warm", "textDocument/definition", definition),
            ("completion_warm", "textDocument/completion", completion),
        ] {
            self.transition(name, |run| {
                let mut measured = Vec::with_capacity(SAMPLES);
                for index in 0..WARMUP + SAMPLES {
                    let (response, duration) = run.client.request(method, params.clone(), None)?;
                    match name {
                        "hover_warm" => {
                            latency::require_hover(&response, latency::SMALL_SIGNATURE, None)?;
                        }
                        "definition_warm" => latency::require_definition(&response, &uris["dep"])?,
                        _ => latency::require_completion(&response)?,
                    }
                    if index >= WARMUP {
                        measured.push(duration);
                    }
                }
                run.result["workloads_ns"][name] = json!(measured);
                run.result["semantics"][name] = json!({"validated_iterations":WARMUP + SAMPLES});
                Ok(())
            })?;
        }
        self.transition("open_to_hover_large", |run| {
            run.result["workloads_ns"]["open_to_hover_large"] = json!(latency::causal_open(
                run.client, workspace, body, SAMPLES, WARMUP
            )?);
            Ok(())
        })?;
        self.transition("edit_to_hover_large", |run| {
            run.result["workloads_ns"]["edit_to_hover_large"] = json!(latency::causal_edit(
                run.client, workspace, body, SAMPLES, WARMUP
            )?);
            Ok(())
        })?;
        self.transition("hover_during_large_edit", |run| {
            run.result["workloads_ns"]["hover_during_large_edit"] = json!(latency::busy_edit(
                run.client,
                workspace,
                &uris["small"],
                body,
                SAMPLES,
                WARMUP
            )?);
            Ok(())
        })?;
        self.transition("hover_during_large_open", |run| {
            run.result["workloads_ns"]["hover_during_large_open"] = json!(latency::busy_open(
                run.client,
                workspace,
                &uris["small"],
                body,
                SAMPLES,
                WARMUP
            )?);
            Ok(())
        })?;
        for name in latency::WORKLOADS {
            if self.result["workloads_ns"][name].as_array().map(Vec::len) != Some(SAMPLES) {
                return Err(format!(
                    "Latency workload {name} did not produce all required samples"
                )
                .into());
            }
            self.result["semantics"][name] =
                json!({"validated_iterations":WARMUP + SAMPLES,"samples":SAMPLES});
        }
        self.transition("latency_close_documents", |run| {
            for uri in uris.values() {
                run.client.close_document(uri)?;
            }
            run.exact_symbols("symbols_after_latency_close", &[])
        })
    }
}

#[derive(Parser)]
#[command(no_binary_name = true)]
struct Args {
    scenario: String,
    #[arg(long)]
    binary: PathBuf,
    #[arg(long)]
    output_dir: PathBuf,
}

pub(crate) fn run(arguments: &[String]) -> ToolResult<()> {
    common::require_ci()?;
    let args = Args::try_parse_from(arguments)?;
    let scenario = Scenario::parse(&args.scenario)?;
    execute(&args.binary, scenario, &args.output_dir, &mut NoProfiler)?;
    Ok(())
}

fn latency_fixture(scenario: Scenario) -> ToolResult<Option<(String, String)>> {
    if scenario == Scenario::Latency {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../benches/fixtures/analyzer_large.bend");
        Ok(Some((
            common::sha256_file(&fixture)?,
            fs::read_to_string(fixture)?.repeat(latency::FIXTURE_REPETITIONS),
        )))
    } else {
        Ok(None)
    }
}

fn initial_result(
    binary: &Path,
    scenario: Scenario,
    dataset_file_count: usize,
    manifest: Value,
    session: &dyn ScenarioSession,
) -> ToolResult<Value> {
    let mut result = json!({
        "format_version":1,"scenario":scenario.name(),"status":"running",
        "dataset_file_count":dataset_file_count,
        "binary_sha256":common::sha256_file(binary)?,
        "samples":SAMPLES,"warmup":WARMUP,
        "phases":[],"timings_ns":{},"requests":[],"semantics":{},"workloads_ns":{},
        "profiler_finish_order":if session.finish_before_shutdown() {"before_child_shutdown"} else {"after_child_shutdown"},
    });
    result["dataset_manifest"] = manifest;
    Ok(result)
}

pub(crate) fn execute(
    binary: &Path,
    scenario: Scenario,
    output_dir: &Path,
    session: &mut dyn ScenarioSession,
) -> ToolResult<Value> {
    common::require_ci()?;
    install_cancellation_handler()?;
    check_cancelled()?;
    let binary = binary.canonicalize()?;
    if !binary.is_file() {
        return Err(format!("LSP binary is not a file: {}", binary.display()).into());
    }
    fs::create_dir_all(output_dir)?;
    let output_dir = output_dir.canonicalize()?;
    let directory = tempfile::Builder::new()
        .prefix("bend-profile-scenario-")
        .tempdir()?;
    let workspace = directory.path().canonicalize()?;
    let (files, manifest) = discovery::dataset(scenario.file_count());
    for (name, source) in &files {
        check_cancelled()?;
        fs::write(workspace.join(name), source)?;
    }
    let (symbols, references) = discovery::expected_results(&workspace, &files)?;
    let latency_body = latency_fixture(scenario)?;
    let mut result = initial_result(&binary, scenario, files.len(), manifest, session)?;
    if let Some((hash, body)) = &latency_body {
        result["latency_fixture"] = json!({"sha256":hash,"repetitions":latency::FIXTURE_REPETITIONS,"expanded_bytes":body.len()});
    }
    let started = Instant::now();
    let mut owner = OwnedSession {
        session,
        client: None,
        complete: false,
    };
    let outcome = (|| -> ToolResult<()> {
        record_phase(&mut result, started, "process_spawn.before")?;
        check_cancelled()?;
        owner.client = Some(LspProcess::spawn(
            &binary,
            &workspace,
            &owner.session.child_environment(),
        )?);
        record_phase(&mut result, started, "process_spawn.after")?;
        let client = owner.client.as_mut().ok_or("Missing spawned LSP")?;
        client.set_cancellation_flag(&CANCELLED);
        client.check_cancelled()?;
        result["pid"] = json!(client.pid());
        record_phase(&mut result, started, "profiler_start.before")?;
        owner.session.started(client.pid())?;
        record_phase(&mut result, started, "profiler_start.after")?;
        let mut run = Workload {
            client,
            session: &mut *owner.session,
            result: &mut result,
            started,
        };
        run.transition("initialize", |run| run.client.initialize())?;
        run.discovery(&workspace, &files, &symbols, &references)?;
        if let Some((_, body)) = &latency_body {
            run.latency(&workspace, body)?;
        }
        if run.session.finish_before_shutdown() {
            run.transition("profiler_finish", |run| run.session.finished())?;
        }
        let finalization_timeout = run.session.finalization_timeout();
        run.transition("shutdown", |run| {
            run.client.finish_profiled(finalization_timeout)
        })?;
        run.result["semantics"]["shutdown"] =
            json!({"null_response":true,"clean_exit":true,"stdout_eof":true});
        if !run.session.finish_before_shutdown() {
            run.transition("profiler_finish", |run| run.session.finished())?;
        }
        Ok(())
    })();
    result["elapsed_ns"] = json!(elapsed(started));
    if let Err(error) = &outcome {
        result["status"] = json!("failed");
        result["error"] = json!(error.to_string());
        record_phase(&mut result, started, "abort.before")?;
        if let Err(cleanup) = owner.abort() {
            result["abort_error"] = json!(cleanup.to_string());
        }
        record_phase(&mut result, started, "abort.after")?;
    } else {
        result["status"] = json!("complete");
    }
    if let Some(client) = &mut owner.client {
        result["shutdown"] = serde_json::to_value(client.shutdown_evidence())?;
        result["stderr_tail"] = json!(
            client
                .stderr_tail()
                .unwrap_or_else(|error| { format!("stderr unavailable: {error}") })
        );
    }
    common::write_json(&output_dir.join("scenario.json"), &result)?;
    outcome?;
    owner.complete = true;
    Ok(result)
}
