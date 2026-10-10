use super::{
    ci_metadata, command_line, executable_identity, hardware_metadata, identity, positive,
    revision, verify_identity,
};
use crate::{
    ToolResult, common, latency,
    transport::{LspProcess, file_uri},
};
use clap::Parser;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const WORKLOADS: [&str; 5] = [
    "hover_warm",
    "definition_warm",
    "references_warm",
    "workspace_symbol_warm",
    "dependency_edit_to_correct_hover",
];

#[derive(Parser)]
#[command(no_binary_name = true)]
struct Args {
    #[arg(long)]
    baseline_binary: PathBuf,
    #[arg(long)]
    candidate_binary: PathBuf,
    #[arg(long, value_parser = revision)]
    baseline_revision: String,
    #[arg(long, value_parser = revision)]
    candidate_revision: String,
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long, default_value = "7", value_parser = positive)]
    rounds: usize,
    #[arg(long, default_value = "32", value_parser = positive)]
    samples: usize,
    #[arg(long, default_value_t = 8)]
    warmup: usize,
    #[arg(long, default_value = "30", value_parser = positive_seconds)]
    discovery_timeout: f64,
}

fn positive_seconds(value: &str) -> Result<f64, String> {
    let seconds: f64 = value.parse().map_err(|_| "Must be finite and positive")?;
    if !seconds.is_finite() || seconds <= 0.0 || Duration::try_from_secs_f64(seconds).is_err() {
        return Err("Must be finite and positive".to_owned());
    }
    Ok(seconds)
}

fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn dataset(count: usize) -> (BTreeMap<String, String>, Value) {
    let mut files = BTreeMap::from([
        (
            "common.bend".to_owned(),
            "def identity(value: U32) -> U32:\n  value\n".to_owned(),
        ),
        (
            "entry.bend".to_owned(),
            "import ./common.bend as Common\ndef main: U32\n  Common.identity(1)\n".to_owned(),
        ),
    ]);
    for index in 0..count.saturating_sub(2) {
        let name = format!("consumer_{index:04}");
        files.insert(format!("{name}.bend"), format!("import ./common.bend as Common\ndef {name}(value: U32) -> U32:\n  Common.identity(value)\n"));
    }
    let manifest: Vec<_> = files.iter().map(|(name, source)| json!({
        "name": name, "bytes": source.len(), "sha256": format!("{:x}", Sha256::digest(source.as_bytes())),
    })).collect();
    (files, json!(manifest))
}

fn location(uri: &str, line: usize, start: usize, end: usize) -> Value {
    json!({"uri": uri, "range": {"start": {"line": line, "character": start}, "end": {"line": line, "character": end}}})
}

fn expected_results(
    workspace: &Path,
    files: &BTreeMap<String, String>,
) -> ToolResult<(Vec<Value>, Vec<Value>)> {
    let mut symbols = Vec::with_capacity(files.len());
    let mut references = Vec::with_capacity(files.len());
    for filename in files.keys() {
        let uri = file_uri(&workspace.join(filename))?;
        let (name, line) = if filename == "common.bend" {
            references.push(location(&uri, 0, 4, 12));
            ("identity", 0)
        } else {
            references.push(location(&uri, 2, 9, 17));
            (
                if filename == "entry.bend" {
                    "main"
                } else {
                    filename.trim_end_matches(".bend")
                },
                1,
            )
        };
        symbols.push(
            json!({"name": name, "kind": 12, "location": location(&uri, line, 4, 4 + name.len())}),
        );
    }
    Ok((symbols, references))
}

fn require_subset(
    result: &Value,
    expected: &[Value],
    label: &str,
    symbols: bool,
    complete: bool,
) -> ToolResult<Value> {
    let empty = Vec::new();
    let items = if result.is_null() && !complete {
        &empty
    } else {
        result
            .as_array()
            .ok_or_else(|| format!("{label}: expected a result list, received {result}"))?
    };
    let expected_keys: BTreeSet<_> = expected.iter().map(Value::to_string).collect();
    let mut keys = BTreeSet::new();
    for item in items {
        if !item.is_object() {
            return Err(format!("{label}: invalid result item {item}").into());
        }
        let normalized = if symbols {
            json!({"name": item["name"], "kind": item["kind"], "location": item["location"]})
        } else {
            item.clone()
        };
        let key = normalized.to_string();
        if !expected_keys.contains(&key) || !keys.insert(key) {
            return Err(
                format!("{label}: duplicate or unexpected semantic result: {result}").into(),
            );
        }
    }
    let ready = keys == expected_keys;
    if complete && !ready {
        return Err(format!(
            "{label}: incomplete workspace: {}/{} results",
            keys.len(),
            expected_keys.len()
        )
        .into());
    }
    Ok(json!({"observed": keys.len(), "expected": expected_keys.len(), "complete": ready}))
}

fn read_memory(pid: u32) -> Result<Value, String> {
    if !cfg!(target_os = "linux") {
        return Err("RSS collection unavailable on this platform; no values inferred".to_owned());
    }
    let text =
        fs::read_to_string(format!("/proc/{pid}/status")).map_err(|error| error.to_string())?;
    let mut result = json!({"rss_bytes": null, "kernel_high_watermark_bytes": null});
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let field = match key {
            "VmRSS" => "rss_bytes",
            "VmHWM" => "kernel_high_watermark_bytes",
            _ => continue,
        };
        let mut parts = value.split_whitespace();
        let number = parts
            .next()
            .ok_or("Missing procfs memory value")?
            .parse::<u64>()
            .map_err(|error| error.to_string())?;
        if parts.next() != Some("kB") || parts.next().is_some() {
            return Err("Unexpected procfs memory unit".to_owned());
        }
        result[field] = json!(number.checked_mul(1024).ok_or("Procfs memory overflow")?);
    }
    Ok(result)
}

struct ResidentMemory {
    pid: u32,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<(Vec<Value>, BTreeSet<String>)>>,
    errors: BTreeSet<String>,
}

impl ResidentMemory {
    fn new(pid: u32) -> Self {
        if !cfg!(target_os = "linux") {
            return Self {
                pid,
                stop: None,
                thread: None,
                errors: BTreeSet::from([
                    "RSS collection unavailable on this platform; no values inferred".to_owned(),
                ]),
            };
        }
        let (stop, receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            let started = Instant::now();
            let mut samples = Vec::new();
            let mut errors = BTreeSet::new();
            loop {
                match read_memory(pid) {
                    Ok(mut value) => {
                        if !value["rss_bytes"].is_null() {
                            value["elapsed_ns"] = json!(elapsed(started));
                            samples.push(value);
                        }
                    }
                    Err(error) => {
                        errors.insert(error);
                    }
                }
                if !matches!(
                    receiver.recv_timeout(Duration::from_millis(20)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
            }
            (samples, errors)
        });
        Self {
            pid,
            stop: Some(stop),
            thread: Some(thread),
            errors: BTreeSet::new(),
        }
    }

    fn checkpoint(&mut self) -> Value {
        if !cfg!(target_os = "linux") {
            return json!({"rss_bytes": null, "kernel_high_watermark_bytes": null});
        }
        match read_memory(self.pid) {
            Ok(value) => value,
            Err(error) => {
                self.errors.insert(error);
                json!({"rss_bytes": null, "kernel_high_watermark_bytes": null})
            }
        }
    }

    fn close(&mut self, result: &mut Value) {
        if let Some(stop) = &self.stop {
            let _ = stop.send(());
        }
        let mut samples = Vec::new();
        if let Some(handle) = self.thread.take() {
            match handle.join() {
                Ok((values, errors)) => {
                    samples = values;
                    self.errors.extend(errors);
                }
                Err(_) => {
                    self.errors
                        .insert("RSS observer panicked; observations unavailable".to_owned());
                }
            }
        }
        result["sampled_peak_rss_bytes"] = json!(
            samples
                .iter()
                .filter_map(|sample| sample["rss_bytes"].as_u64())
                .max()
        );
        result["rss_samples"] = json!(samples);
        result["memory_errors"] = json!(self.errors);
    }
}

impl Drop for ResidentMemory {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            let _ = stop.send(());
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

fn request(
    client: &mut LspProcess,
    result: &mut Value,
    name: &str,
    method: &str,
    params: Value,
    notification: Option<Value>,
) -> ToolResult<Value> {
    let (value, duration) = client.request(method, params, notification)?;
    result["timings_ns"][name] = json!(duration);
    Ok(value)
}

fn warm(
    client: &mut LspProcess,
    result: &mut Value,
    args: &Args,
    name: &str,
    method: &str,
    params: &Value,
    check: impl Fn(&Value) -> ToolResult<()>,
) -> ToolResult<()> {
    result["workloads_ns"][name] = json!([]);
    for index in 0..args
        .warmup
        .checked_add(args.samples)
        .ok_or("Sample count overflow")?
    {
        let (value, duration) = client.request(method, params.clone(), None)?;
        check(&value)?;
        if index >= args.warmup {
            result["workloads_ns"][name]
                .as_array_mut()
                .ok_or("Missing workload samples")?
                .push(json!(duration));
        }
    }
    Ok(())
}

fn measure_round(
    binary: &Path,
    files: &BTreeMap<String, String>,
    candidate: bool,
    args: &Args,
    result: &mut Value,
) -> ToolResult<()> {
    let directory = tempfile::Builder::new()
        .prefix("bend-discovery-")
        .tempdir()?;
    let workspace = directory.path().canonicalize()?;
    for (name, source) in files {
        fs::write(workspace.join(name), source)?;
    }
    let (expected_symbols, expected_refs) = expected_results(&workspace, files)?;
    result["status"] = json!("running");
    for key in [
        "timings_ns",
        "memory_checkpoints",
        "semantics",
        "workloads_ns",
    ] {
        result[key] = json!({});
    }
    result["discovery_polls"] = json!([]);
    let spawn_start = Instant::now();
    let mut client = match LspProcess::spawn(binary, &workspace, &[]) {
        Ok(client) => client,
        Err(error) => {
            result["status"] = json!("failed");
            result["error"] = json!(error.to_string());
            return Err(error);
        }
    };
    result["timings_ns"]["process_spawn"] = json!(elapsed(spawn_start));
    let mut memory = ResidentMemory::new(client.pid());
    let outcome = Round {
        client: &mut client,
        memory: &mut memory,
        result,
        args,
        files,
        candidate,
        workspace: &workspace,
        expected_symbols: &expected_symbols,
        expected_refs: &expected_refs,
        spawn_start,
    }
    .run();
    if let Err(error) = &outcome {
        result["status"] = json!("failed");
        result["error"] = json!(error.to_string());
        result["stderr_tail"] = json!(
            client
                .stderr_tail()
                .unwrap_or_else(|failure| format!("stderr unavailable: {failure}"))
        );
    } else {
        result["status"] = json!("complete");
    }
    memory.close(result);
    outcome
}

struct Round<'a> {
    client: &'a mut LspProcess,
    memory: &'a mut ResidentMemory,
    result: &'a mut Value,
    args: &'a Args,
    files: &'a BTreeMap<String, String>,
    candidate: bool,
    workspace: &'a Path,
    expected_symbols: &'a [Value],
    expected_refs: &'a [Value],
    spawn_start: Instant,
}

impl Round<'_> {
    fn request(
        &mut self,
        name: &str,
        method: &str,
        params: Value,
        notification: Option<Value>,
    ) -> ToolResult<Value> {
        request(self.client, self.result, name, method, params, notification)
    }

    fn run(&mut self) -> ToolResult<()> {
        let initialized = self.initialize()?;
        let symbols_ready = self.cold_symbols(initialized)?;
        let common_uri = file_uri(&self.workspace.join("common.bend"))?;
        let entry_uri = file_uri(&self.workspace.join("entry.bend"))?;
        self.cold_references(&common_uri, initialized, symbols_ready)?;
        let hover_params = latency::position(&entry_uri, 2, 12);
        let mut refs_params = hover_params.clone();
        refs_params["context"] = json!({"includeDeclaration": true});
        self.open_entry(&entry_uri, &hover_params, &refs_params)?;
        self.warm_queries(&common_uri, &hover_params, &refs_params)?;
        self.dependency_revisions(&common_uri, &hover_params)?;
        let refs = self.request(
            "references_after_dependency_revisions",
            "textDocument/references",
            refs_params,
            None,
        )?;
        self.result["semantics"]["final_references"] = require_subset(
            &refs,
            self.expected_refs,
            "final references",
            false,
            self.candidate,
        )?;
        let symbols = self.request(
            "symbols_after_dependency_revisions",
            "workspace/symbol",
            json!({"query": ""}),
            None,
        )?;
        self.result["semantics"]["final_symbols"] = require_subset(
            &symbols,
            self.expected_symbols,
            "final symbols",
            true,
            self.candidate,
        )?;
        if WORKLOADS.iter().any(|name| {
            self.result["workloads_ns"][name].as_array().map(Vec::len) != Some(self.args.samples)
        }) {
            return Err("Round did not produce every required sample".into());
        }
        self.client.finish()
    }

    fn initialize(&mut self) -> ToolResult<Instant> {
        let root_uri = file_uri(self.workspace)?;
        let capabilities = self.request(
            "initialize_request",
            "initialize",
            json!({
                "processId": null, "rootUri": root_uri, "capabilities": {},
                "workspaceFolders": [{"uri": root_uri, "name": "discovery"}]
            }),
            None,
        )?;
        if !capabilities["capabilities"].is_object() {
            return Err("Initialize did not return capabilities".into());
        }
        self.result["timings_ns"]["spawn_to_initialize_response"] =
            json!(elapsed(self.spawn_start));
        self.result["memory_checkpoints"]["initialize_response"] = self.memory.checkpoint();
        let initialized = Instant::now();
        self.client.notify("initialized", json!({}))?;
        self.client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": self.client.settings()}),
        )?;
        Ok(initialized)
    }

    fn cold_symbols(&mut self, initialized: Instant) -> ToolResult<bool> {
        let mut symbols = self.request(
            "first_workspace_symbol",
            "workspace/symbol",
            json!({"query": ""}),
            None,
        )?;
        self.result["timings_ns"]["initialized_to_first_workspace_symbol_response"] =
            json!(elapsed(initialized));
        self.result["first_workspace_symbol_result"] = symbols.clone();
        let deadline = Instant::now()
            .checked_add(Duration::try_from_secs_f64(self.args.discovery_timeout)?)
            .ok_or("Discovery timeout overflow")?;
        let scope = loop {
            let scope =
                require_subset(&symbols, self.expected_symbols, "cold symbols", true, false)?;
            let mut observation = scope.clone();
            observation["since_initialized_ns"] = json!(elapsed(initialized));
            self.result["discovery_polls"]
                .as_array_mut()
                .ok_or("Missing discovery polls")?
                .push(observation);
            self.result["semantics"]["cold_symbols"] = scope.clone();
            self.result["cold_workspace_symbol_result"] = symbols;
            if scope["complete"] == true || !self.candidate {
                break scope;
            }
            if Instant::now() >= deadline {
                return Err("Candidate discovery did not expose the complete symbol set".into());
            }
            thread::sleep(Duration::from_millis(50));
            symbols = self
                .client
                .request("workspace/symbol", json!({"query": ""}), None)?
                .0;
        };
        let ready = scope["complete"] == true;
        self.result["timings_ns"]["whole_workspace_symbols_ready"] = if ready {
            json!(elapsed(initialized))
        } else {
            Value::Null
        };
        Ok(ready)
    }

    fn cold_references(
        &mut self,
        common_uri: &str,
        initialized: Instant,
        symbols_ready: bool,
    ) -> ToolResult<()> {
        let mut params = latency::position(common_uri, 0, 7);
        params["context"] = json!({"includeDeclaration": true});
        let refs = self.request("cold_references", "textDocument/references", params, None)?;
        self.result["cold_references_result"] = refs.clone();
        let scope = require_subset(&refs, self.expected_refs, "cold references", false, false)?;
        self.result["semantics"]["cold_references"] = scope.clone();
        if self.candidate && scope["complete"] != true {
            return Err("Cold references: incomplete candidate workspace".into());
        }
        self.result["timings_ns"]["whole_workspace_discovery_ready"] =
            if symbols_ready && scope["complete"] == true {
                json!(elapsed(initialized))
            } else {
                Value::Null
            };
        self.result["memory_checkpoints"]["cold_discovery_observed"] = self.memory.checkpoint();
        Ok(())
    }

    fn open_entry(
        &mut self,
        entry_uri: &str,
        hover_params: &Value,
        refs_params: &Value,
    ) -> ToolResult<()> {
        let source = self.files.get("entry.bend").ok_or("Missing entry source")?;
        let notification = latency::open_message(entry_uri, source, 1);
        let hover = self.request(
            "open_entry_to_correct_hover",
            "textDocument/hover",
            hover_params.clone(),
            Some(notification),
        )?;
        latency::require_hover(&hover, "def identity(value: U32) -> U32", None)?;
        self.client.wait_diagnostics(entry_uri, Some(1))?;
        let refs = self.request(
            "first_references_after_open",
            "textDocument/references",
            refs_params.clone(),
            None,
        )?;
        self.result["references_after_open_result"] = refs.clone();
        self.result["semantics"]["references_after_open"] = require_subset(
            &refs,
            self.expected_refs,
            "references after open",
            false,
            self.candidate,
        )?;
        let symbols = self.request(
            "symbols_after_open",
            "workspace/symbol",
            json!({"query": ""}),
            None,
        )?;
        self.result["semantics"]["symbols_after_open"] = require_subset(
            &symbols,
            self.expected_symbols,
            "symbols after open",
            true,
            self.candidate,
        )?;
        self.result["memory_checkpoints"]["entry_loaded"] = self.memory.checkpoint();
        Ok(())
    }

    fn warm_queries(
        &mut self,
        common_uri: &str,
        hover_params: &Value,
        refs_params: &Value,
    ) -> ToolResult<()> {
        let expected_refs = self.expected_refs;
        let expected_symbols = self.expected_symbols;
        let candidate = self.candidate;
        warm(
            self.client,
            self.result,
            self.args,
            "hover_warm",
            "textDocument/hover",
            hover_params,
            |value| latency::require_hover(value, "def identity(value: U32) -> U32", None),
        )?;
        warm(
            self.client,
            self.result,
            self.args,
            "definition_warm",
            "textDocument/definition",
            hover_params,
            |value| {
                if *value != location(common_uri, 0, 4, 12) {
                    return Err(format!("Incorrect definition: {value}").into());
                }
                Ok(())
            },
        )?;
        warm(
            self.client,
            self.result,
            self.args,
            "references_warm",
            "textDocument/references",
            refs_params,
            |value| {
                require_subset(value, expected_refs, "warm references", false, candidate)
                    .map(|_| ())
            },
        )?;
        warm(
            self.client,
            self.result,
            self.args,
            "workspace_symbol_warm",
            "workspace/symbol",
            &json!({"query": ""}),
            |value| {
                require_subset(value, expected_symbols, "warm symbols", true, candidate).map(|_| ())
            },
        )?;
        Ok(())
    }

    fn dependency_revisions(&mut self, common_uri: &str, hover_params: &Value) -> ToolResult<()> {
        let source = self
            .files
            .get("common.bend")
            .ok_or("Missing common source")?;
        self.client.notify(
            "textDocument/didOpen",
            latency::open_message(common_uri, source, 1)["params"].clone(),
        )?;
        self.client.wait_diagnostics(common_uri, Some(1))?;
        self.result["workloads_ns"]["dependency_edit_to_correct_hover"] = json!([]);
        for index in 0..self
            .args
            .warmup
            .checked_add(self.args.samples)
            .ok_or("Sample count overflow")?
        {
            let (kind, old_kind) = if index % 2 == 0 {
                ("U64", "U32")
            } else {
                ("U32", "U64")
            };
            let version = i64::try_from(index)?
                .checked_add(2)
                .ok_or("Revision overflow")?;
            let source = source.replace("U32", kind);
            let (value, duration) = self.client.request(
                "textDocument/hover",
                hover_params.clone(),
                Some(latency::change_message(common_uri, &source, version)),
            )?;
            latency::require_hover(
                &value,
                &format!("def identity(value: {kind}) -> {kind}"),
                Some(&format!("def identity(value: {old_kind}) -> {old_kind}")),
            )?;
            if index >= self.args.warmup {
                self.result["workloads_ns"]["dependency_edit_to_correct_hover"]
                    .as_array_mut()
                    .ok_or("Missing edit samples")?
                    .push(json!(duration));
            }
            self.client.wait_diagnostics(common_uri, Some(version))?;
        }
        self.result["memory_checkpoints"]["after_dependency_revisions"] = self.memory.checkpoint();
        Ok(())
    }
}

pub(super) fn run(arguments: &[String]) -> ToolResult<()> {
    let args = Args::try_parse_from(arguments)?;
    common::require_ci()?;
    let target = common::native_target()?;
    if args.output_dir.join("raw.json").exists() {
        return Err(
            "Output directory already contains evidence; failed evidence is never replaced".into(),
        );
    }
    fs::create_dir_all(&args.output_dir)?;
    let output = args.output_dir.canonicalize()?;
    let binaries = BTreeMap::from([
        ("baseline", args.baseline_binary.canonicalize()?),
        ("candidate", args.candidate_binary.canonicalize()?),
    ]);
    if binaries.values().any(|binary| binary.starts_with(&output)) {
        return Err("Output directory must not contain either input binary".into());
    }
    let harness = executable_identity()?;
    let mut data = json!({"format_version": 1, "status": "running", "metadata": {
        "command": command_line(), "platform": std::env::consts::OS, "machine": std::env::consts::ARCH,
        "native_target": target, "hardware": hardware_metadata(), "ci": ci_metadata(),
        "rounds": args.rounds, "samples": args.samples, "warmup": args.warmup,
        "discovery_timeout_seconds": args.discovery_timeout,
        "baseline_revision": args.baseline_revision, "candidate_revision": args.candidate_revision,
        "candidate_revision_kind": "workflow checkout revision (PR merge commit in pull_request workflows)",
        "harness": harness, "binaries": {}, "round_order": round_order(args.rounds),
        "timing": "Instant nanoseconds; request write start through full body receipt before JSON parsing; causal edits include notification write",
        "cold_timing": "readiness includes dispatch, semantic validation and polling; no buffer opened before cold symbol/reference checks",
        "baseline_readiness": "first public cold observation only; missing discovery is neither polled to timeout nor treated as complete",
        "compiler": "unavailable; isolated PATH/HOME; BEND_LIB/trace/metrics removed; analysis/protocol only",
        "memory": "Linux procfs VmRSS/VmHWM only; unsupported platforms null; sampled every 20ms; sampled peak is a lower bound, not allocations"
    }, "datasets": {}});
    persist(&output, &data)?;
    let outcome = collect(&args, &binaries, &output, &mut data);
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

fn round_order(rounds: usize) -> Vec<[&'static str; 2]> {
    (0..rounds)
        .map(|index| {
            if index % 2 == 0 {
                ["baseline", "candidate"]
            } else {
                ["candidate", "baseline"]
            }
        })
        .collect()
}

fn collect(
    args: &Args,
    binaries: &BTreeMap<&str, PathBuf>,
    output: &Path,
    data: &mut Value,
) -> ToolResult<()> {
    for (variant, binary) in binaries {
        data["metadata"]["binaries"][variant] = identity(binary)?;
    }
    for count in [10, 1000] {
        let (files, manifest) = dataset(count);
        let key = count.to_string();
        let root = output.join("datasets").join(&key);
        fs::create_dir_all(&root)?;
        for (name, source) in &files {
            fs::write(root.join(name), source)?;
        }
        common::write_json(&root.join("manifest.json"), &manifest)?;
        data["datasets"][&key] = json!({
            "dataset_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(&manifest)?)),
            "source_bytes": files.values().map(String::len).sum::<usize>(), "manifest": manifest,
            "variants": {"baseline": [], "candidate": []}
        });
        persist(output, data)?;
        for (index, order) in round_order(args.rounds).iter().enumerate() {
            for variant in order {
                let binary = binaries.get(variant).ok_or("Missing variant binary")?;
                data["datasets"][&key]["variants"][variant]
                    .as_array_mut()
                    .ok_or("Missing variant rounds")?
                    .push(json!({"round": index + 1, "status": "running"}));
                persist(output, data)?;
                let result = data["datasets"][&key]["variants"][variant]
                    .as_array_mut()
                    .and_then(|rounds| rounds.last_mut())
                    .ok_or("Missing current round")?;
                let outcome = measure_round(binary, &files, *variant == "candidate", args, result);
                if let Err(error) = &outcome {
                    result["status"] = json!("failed");
                    result["error"] = json!(error.to_string());
                }
                persist(output, data)?;
                outcome?;
            }
        }
    }
    verify_identity(&data["metadata"]["harness"])?;
    for variant in ["baseline", "candidate"] {
        verify_identity(&data["metadata"]["binaries"][variant])?;
    }
    Ok(())
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    Some(if values.len().is_multiple_of(2) {
        values[middle - 1] / 2.0 + values[middle] / 2.0
    } else {
        values[middle]
    })
}

fn observations(rounds: &[&Value], selector: impl Fn(&Value) -> &Value) -> Value {
    let values: Option<Vec<f64>> = rounds
        .iter()
        .map(|round| selector(round).as_f64())
        .collect();
    json!(values.and_then(median))
}

fn summarize(rounds: &[&Value], workload: &str) -> ToolResult<Value> {
    let mut p50 = Vec::new();
    let mut p95 = Vec::new();
    let mut all = Vec::new();
    for round in rounds {
        let mut values: Vec<_> = round["workloads_ns"][workload]
            .as_array()
            .ok_or("Missing completed workload")?
            .iter()
            .map(|value| value.as_u64().ok_or("Invalid workload timing"))
            .collect::<Result<_, _>>()?;
        if values.is_empty() {
            return Err("Empty completed workload".into());
        }
        values.sort_unstable();
        p50.push(
            serde_json::Number::from(values[values.len().div_ceil(2) - 1])
                .as_f64()
                .ok_or("Invalid percentile")?,
        );
        p95.push(
            serde_json::Number::from(values[(values.len() * 95).div_ceil(100) - 1])
                .as_f64()
                .ok_or("Invalid percentile")?,
        );
        all.push(json!({"p50_ns": values[values.len().div_ceil(2) - 1], "p95_ns": values[(values.len() * 95).div_ceil(100) - 1]}));
    }
    Ok(json!({"p50_ns": median(p50), "p95_ns": median(p95), "rounds": all}))
}

fn report(data: &Value) -> ToolResult<(Value, String)> {
    let mut output = json!({
        "format_version": 1, "mode": "report-only", "status": data["status"],
        "metadata": data["metadata"], "datasets": {}
    });
    let mut markdown = format!(
        "# Whole-workspace discovery and warm LSP queries\n\nCollection status: **{}**.\n\n\
         Report-only: no numerical regression gate. Cold protocol readiness differs from exact \
         complete symbols and references before opening buffers. Baseline absence is recorded, \
         not failed. Incomplete baseline references/symbols are different work, not comparable \
         latencies. Warm p50/p95 are medians of nearest-rank per-round percentiles, not pooled. \
         Cold times and RSS are medians of completed rounds. Linux /proc RSS samples every 20ms \
         are a lower bound, not allocation peaks; other platforms report null. Fresh \
         processes/workspaces do not flush OS caches.\n",
        data["status"].as_str().unwrap_or("unknown")
    );
    if let Some(error) = data["error"].as_str() {
        writeln!(
            markdown,
            "\nFailure (partial raw evidence retained):\n```text\n{error}\n```"
        )?;
    }
    for (count, section) in data["datasets"].as_object().ok_or("Missing datasets")? {
        let summary = dataset_summary(section)?;
        writeln!(
            markdown,
            "\n## {count} files\n\n| Variant | Completed rounds | Cold symbols | Cold references |\n| --- | ---: | --- | --- |"
        )?;
        for variant in ["baseline", "candidate"] {
            let all = section["variants"][variant]
                .as_array()
                .ok_or("Missing variant")?;
            writeln!(
                markdown,
                "| {variant} | {}/{} | {} | {} |",
                summary[variant]["completed_rounds"],
                data["metadata"]["rounds"],
                completeness(all, "cold_symbols"),
                completeness(all, "cold_references")
            )?;
        }
        write_cold_table(&mut markdown, &summary)?;
        write_warm_table(&mut markdown, &summary, section)?;
        write_memory_table(&mut markdown, &summary)?;
        output["datasets"][count] = summary;
    }
    writeln!(
        markdown,
        "\n## Provenance\n\n```json\n{}\n```",
        serde_json::to_string_pretty(&data["metadata"])?
    )?;
    Ok((output, markdown))
}

fn completeness(rounds: &[Value], name: &str) -> String {
    let observations: BTreeSet<_> = rounds
        .iter()
        .filter_map(|round| round["semantics"][name].as_object())
        .map(|scope| format!("{}/{}", scope["observed"], scope["expected"]))
        .collect();
    if observations.is_empty() {
        "not observed".to_owned()
    } else {
        observations.into_iter().collect::<Vec<_>>().join(", ")
    }
}

fn dataset_summary(section: &Value) -> ToolResult<Value> {
    let mut summary = json!({});
    for variant in ["baseline", "candidate"] {
        let all = section["variants"][variant]
            .as_array()
            .ok_or("Missing variant")?;
        let rounds: Vec<_> = all
            .iter()
            .filter(|round| round["status"] == "complete")
            .collect();
        let mut value = json!({
            "completed_rounds": rounds.len(), "workloads": {}, "cold_medians_ns": {},
            "memory_medians_bytes": {},
            "semantic_observations": all.iter().map(|round| &round["semantics"]).collect::<Vec<_>>()
        });
        if let Some(first) = rounds.first() {
            for workload in WORKLOADS {
                value["workloads"][workload] = summarize(&rounds, workload)?;
            }
            for name in first["timings_ns"]
                .as_object()
                .ok_or("Missing timing fields")?
                .keys()
            {
                value["cold_medians_ns"][name] =
                    observations(&rounds, |round| &round["timings_ns"][name]);
            }
            value["memory_medians_bytes"]["sampled_peak_rss"] =
                observations(&rounds, |round| &round["sampled_peak_rss_bytes"]);
            for name in first["memory_checkpoints"]
                .as_object()
                .ok_or("Missing memory checkpoints")?
                .keys()
            {
                for field in ["rss_bytes", "kernel_high_watermark_bytes"] {
                    value["memory_medians_bytes"][format!("{name}.{field}")] =
                        observations(&rounds, |round| &round["memory_checkpoints"][name][field]);
                }
            }
        }
        summary[variant] = value;
    }
    Ok(summary)
}

fn write_cold_table(markdown: &mut String, summary: &Value) -> ToolResult<()> {
    markdown.push_str("\n| Cold observation | Baseline median (ms) | Candidate median (ms) |\n| --- | ---: | ---: |\n");
    for name in [
        "spawn_to_initialize_response",
        "first_workspace_symbol",
        "initialized_to_first_workspace_symbol_response",
        "whole_workspace_symbols_ready",
        "whole_workspace_discovery_ready",
        "open_entry_to_correct_hover",
    ] {
        writeln!(
            markdown,
            "| {name} | {} | {} |",
            cell(&summary["baseline"]["cold_medians_ns"][name], 1_000_000.0),
            cell(&summary["candidate"]["cold_medians_ns"][name], 1_000_000.0)
        )?;
    }
    Ok(())
}

fn write_warm_table(markdown: &mut String, summary: &Value, section: &Value) -> ToolResult<()> {
    markdown.push_str("\n| Warm workload | Baseline p50 / p95 (ms) | Candidate p50 / p95 (ms) | Scope |\n| --- | ---: | ---: | --- |\n");
    for name in WORKLOADS {
        let scope_key = match name {
            "references_warm" => Some("references_after_open"),
            "workspace_symbol_warm" => Some("symbols_after_open"),
            _ => None,
        };
        let incomplete = scope_key.is_some_and(|key| {
            section["variants"]["baseline"]
                .as_array()
                .is_none_or(|rounds| {
                    rounds.is_empty()
                        || rounds
                            .iter()
                            .any(|round| round["semantics"][key]["complete"] != true)
                })
        });
        let scope = if incomplete {
            "changed/incomplete baseline scope; not comparable"
        } else {
            "same checked answer"
        };
        writeln!(
            markdown,
            "| {name} | {} / {} | {} / {} | {scope} |",
            cell(
                &summary["baseline"]["workloads"][name]["p50_ns"],
                1_000_000.0
            ),
            cell(
                &summary["baseline"]["workloads"][name]["p95_ns"],
                1_000_000.0
            ),
            cell(
                &summary["candidate"]["workloads"][name]["p50_ns"],
                1_000_000.0
            ),
            cell(
                &summary["candidate"]["workloads"][name]["p95_ns"],
                1_000_000.0
            )
        )?;
    }
    Ok(())
}

fn write_memory_table(markdown: &mut String, summary: &Value) -> ToolResult<()> {
    markdown.push_str("\n| Resident memory | Baseline median (MiB) | Candidate median (MiB) |\n| --- | ---: | ---: |\n");
    let mut names = BTreeSet::new();
    for variant in ["baseline", "candidate"] {
        if let Some(fields) = summary[variant]["memory_medians_bytes"].as_object() {
            names.extend(fields.keys());
        }
    }
    for name in names {
        writeln!(
            markdown,
            "| {name} | {} | {} |",
            cell(
                &summary["baseline"]["memory_medians_bytes"][name],
                1_048_576.0
            ),
            cell(
                &summary["candidate"]["memory_medians_bytes"][name],
                1_048_576.0
            )
        )?;
    }
    Ok(())
}

fn cell(value: &Value, divisor: f64) -> String {
    value.as_f64().map_or_else(
        || "unavailable/incomplete".to_owned(),
        |number| format!("{:.3}", number / divisor),
    )
}

fn persist(output: &Path, data: &Value) -> ToolResult<()> {
    common::write_json(&output.join("raw.json"), data)?;
    let (summary, markdown) = report(data)?;
    common::write_json(&output.join("report.json"), &summary)?;
    fs::write(output.join("report.md"), markdown)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_deterministic_source_manifests() -> ToolResult<()> {
        for (count, bytes, digest) in [
            (
                10,
                859,
                "557e94038a9b1e4341177994b8c82bb1c8bed724c455e2e857e0aa8011b972cb",
            ),
            (
                1000,
                93919,
                "a2fda767823537ee20d3455f1edc28c7a53f1996ddcfd3ae90ee13455dc62fb8",
            ),
        ] {
            let (files, manifest) = dataset(count);
            assert_eq!(files.len(), count);
            assert_eq!(manifest.as_array().map(Vec::len), Some(count));
            assert_eq!(dataset(count), (files.clone(), manifest.clone()));
            assert_eq!(files.values().map(String::len).sum::<usize>(), bytes);
            assert_eq!(
                format!("{:x}", Sha256::digest(serde_json::to_vec(&manifest)?)),
                digest
            );
            assert_eq!(
                files["common.bend"],
                "def identity(value: U32) -> U32:\n  value\n"
            );
            assert_eq!(
                files["entry.bend"],
                "import ./common.bend as Common\ndef main: U32\n  Common.identity(1)\n"
            );
            for item in manifest.as_array().ok_or("Missing manifest")? {
                let source = files
                    .get(item["name"].as_str().ok_or("Missing manifest name")?)
                    .ok_or("Missing source")?;
                assert_eq!(item["bytes"], source.len());
                assert_eq!(
                    item["sha256"],
                    format!("{:x}", Sha256::digest(source.as_bytes()))
                );
            }
        }
        Ok(())
    }

    #[test]
    fn baseline_absence_never_accepts_wrong_or_duplicate_answers() -> ToolResult<()> {
        let expected = vec![location("file:///common.bend", 0, 4, 12)];
        assert_eq!(
            require_subset(&Value::Null, &expected, "baseline", false, false)?["complete"],
            false
        );
        assert!(require_subset(&Value::Null, &expected, "candidate", false, true).is_err());
        assert!(require_subset(&json!([]), &expected, "candidate", false, true).is_err());
        assert!(
            require_subset(
                &json!([expected[0], expected[0]]),
                &expected,
                "baseline",
                false,
                false
            )
            .is_err()
        );
        assert!(
            require_subset(
                &json!([location("file:///common.bend", 0, 4, 11)]),
                &expected,
                "baseline",
                false,
                false
            )
            .is_err()
        );
        assert_eq!(
            require_subset(&json!(expected), &expected, "candidate", false, true)?["complete"],
            true
        );
        Ok(())
    }

    #[test]
    fn reports_only_completed_rounds_and_preserves_missing_readiness() -> ToolResult<()> {
        let workloads: BTreeMap<_, _> = WORKLOADS
            .into_iter()
            .map(|name| (name, json!([1, 2, 100])))
            .collect();
        let complete = json!({"status": "complete", "workloads_ns": workloads, "timings_ns": {"whole_workspace_discovery_ready": null}, "sampled_peak_rss_bytes": null, "memory_checkpoints": {"initialize_response": {"rss_bytes": null, "kernel_high_watermark_bytes": null}}, "semantics": {"cold_symbols": {"observed": 0, "expected": 10, "complete": false}, "references_after_open": {"complete": false}}});
        let data = json!({"status": "failed", "metadata": {"rounds": 2}, "datasets": {"10": {"variants": {"baseline": [complete, {"status": "failed", "semantics": {"cold_symbols": {"observed": 1, "expected": 10, "complete": false}}}], "candidate": []}}}});
        let (report, markdown) = report(&data)?;
        assert_eq!(report["datasets"]["10"]["baseline"]["completed_rounds"], 1);
        assert_eq!(
            report["datasets"]["10"]["baseline"]["workloads"]["hover_warm"]["p50_ns"],
            2.0
        );
        assert_eq!(
            report["datasets"]["10"]["baseline"]["workloads"]["hover_warm"]["p95_ns"],
            100.0
        );
        assert!(report["datasets"]["10"]["baseline"]["cold_medians_ns"]["whole_workspace_discovery_ready"].is_null());
        assert!(markdown.contains("not comparable"));
        assert_eq!(
            round_order(3),
            [
                ["baseline", "candidate"],
                ["candidate", "baseline"],
                ["baseline", "candidate"]
            ]
        );
        Ok(())
    }

    #[test]
    fn cli_rejects_invalid_revisions_counts_and_timeouts() {
        let base = [
            "--baseline-binary",
            "baseline",
            "--candidate-binary",
            "candidate",
            "--baseline-revision",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "--candidate-revision",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "--output-dir",
            "output",
        ];
        assert!(Args::try_parse_from(base).is_ok());
        for (flag, value) in [
            ("--rounds", "0"),
            ("--samples", "0"),
            ("--discovery-timeout", "NaN"),
            ("--discovery-timeout", "inf"),
            ("--warmup", "-1"),
        ] {
            assert!(Args::try_parse_from(base.into_iter().chain([flag, value])).is_err());
        }
        assert!(revision("main").is_err());
    }
}
