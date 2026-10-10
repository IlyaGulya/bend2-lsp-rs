use crate::{
    ToolResult, common,
    transport::{LspProcess, REQUEST_TIMEOUT, file_uri},
};
use clap::Parser;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub(crate) const WORKLOADS: [&str; 7] = [
    "hover_warm",
    "definition_warm",
    "completion_warm",
    "open_to_hover_large",
    "edit_to_hover_large",
    "hover_during_large_edit",
    "hover_during_large_open",
];
pub(crate) const FIXTURE_REPETITIONS: usize = 4;
pub(crate) const ADT_SOURCE: &str =
    "type LatencyTerm is Data:\n  TermVar{index: Nat}\n  TermRef{name: String}\n";
pub(crate) const SMALL_SOURCE: &str = concat!(
    "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  add(1, 2)\n",
    "type LatencyTerm is Data:\n  TermVar{index: Nat}\n  TermRef{name: String}\n"
);
pub(crate) const SMALL_SIGNATURE: &str = "def add(x: U32, y: U32) -> U32";
pub(crate) const DEPENDENCY_SOURCE: &str = concat!(
    "def clamp(x: U32) -> U32:\n  x\n",
    "type LatencyTerm is Data:\n  TermVar{index: Nat}\n  TermRef{name: String}\n"
);
pub(crate) const IMPORTER_SOURCE: &str = concat!(
    "import dep.bend as Dep\ndef main: U32\n  Dep.clamp(1)\n",
    "type LatencyTerm is Data:\n  TermVar{index: Nat}\n  TermRef{name: String}\n"
);

pub(crate) fn position(uri: &str, line: u32, character: u32) -> Value {
    json!({"textDocument":{"uri":uri},"position":{"line":line,"character":character}})
}

pub(crate) fn open_message(uri: &str, source: &str, version: i64) -> Value {
    json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":"bend","version":version,"text":source}}})
}

pub(crate) fn change_message(uri: &str, source: &str, version: i64) -> Value {
    json!({"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":source}]}})
}

pub(crate) fn require_hover(
    result: &Value,
    signature: &str,
    previous: Option<&str>,
) -> ToolResult<()> {
    let value = result["contents"]["value"]
        .as_str()
        .ok_or("hover did not return markup contents")?;
    if !value.contains(signature) {
        return Err(format!("hover did not expose {signature:?}: {result}").into());
    }
    if previous.is_some_and(|previous| value.contains(previous)) {
        return Err(format!("hover exposed stale revision: {result}").into());
    }
    Ok(())
}

pub(crate) fn require_definition(result: &Value, uri: &str) -> ToolResult<()> {
    let expected = json!({"uri":uri,"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":9}}});
    if result != &expected {
        return Err(format!("definition expected {expected}, received {result}").into());
    }
    Ok(())
}

pub(crate) fn require_completion(result: &Value) -> ToolResult<()> {
    let items = result.get("items").unwrap_or(result);
    if !items
        .as_array()
        .is_some_and(|items| items.iter().any(|item| item["label"] == "add"))
    {
        return Err(format!("completion did not offer known add declaration: {result}").into());
    }
    Ok(())
}

fn large_revision(body: &str, workload: &str, revision: usize) -> (String, String) {
    let name = format!("latency_{workload}_revision_{revision}");
    let source = format!("def {name}: U32\n  {revision}\n{ADT_SOURCE}{body}");
    (name, source)
}

fn send_notification(client: &mut LspProcess, notification: &Value) -> ToolResult<()> {
    let method = notification["method"]
        .as_str()
        .ok_or("notification has no method")?;
    client.notify(method, notification["params"].clone())
}

fn warm_queries(
    client: &mut LspProcess,
    method: &str,
    params: &Value,
    samples: usize,
    warmup: usize,
    check: impl Fn(&Value) -> ToolResult<()>,
) -> ToolResult<Vec<u64>> {
    let mut measured = Vec::with_capacity(samples);
    for index in 0..warmup + samples {
        let (result, elapsed) = client.request(method, params.clone(), None)?;
        check(&result)?;
        if index >= warmup {
            measured.push(elapsed);
        }
    }
    Ok(measured)
}

fn pipelined_hovers(
    client: &mut LspProcess,
    uri: &str,
    count: usize,
    notification: &Value,
) -> ToolResult<Vec<u64>> {
    let prepared = (0..count)
        .map(|_| client.prepare_request("textDocument/hover", position(uri, 3, 4)))
        .collect::<ToolResult<Vec<_>>>()?;
    send_notification(client, notification)?;
    let pending = prepared
        .into_iter()
        .map(|request| client.send_request(request, None))
        .collect::<ToolResult<Vec<_>>>()?;
    let mut measured = Vec::with_capacity(count);
    for request in pending {
        let (result, elapsed) = client.response(request)?;
        require_hover(&result, SMALL_SIGNATURE, None)?;
        measured.push(elapsed);
    }
    Ok(measured)
}

fn workloads(
    client: &mut LspProcess,
    workspace: &Path,
    body: &str,
    samples: usize,
    warmup: usize,
) -> ToolResult<BTreeMap<String, Vec<u64>>> {
    let completion_source = SMALL_SOURCE.replace("  add(1, 2)\n", "  ad\n");
    let documents = [
        ("small", SMALL_SOURCE),
        ("completion", &completion_source),
        ("dep", DEPENDENCY_SOURCE),
        ("importer", IMPORTER_SOURCE),
    ];
    let mut uris = BTreeMap::new();
    for (name, source) in documents {
        let path = workspace.join(format!("{name}.bend"));
        fs::write(&path, source)?;
        uris.insert(name, file_uri(&path)?);
    }
    client.initialize()?;
    for (name, source) in documents {
        send_notification(client, &open_message(&uris[name], source, 1))?;
        client.wait_diagnostics(&uris[name], Some(1))?;
    }
    let small = position(&uris["small"], 3, 4);
    let definition = position(&uris["importer"], 2, 8);
    let completion = position(&uris["completion"], 3, 4);
    require_hover(
        &client.request("textDocument/hover", small.clone(), None)?.0,
        SMALL_SIGNATURE,
        None,
    )?;
    require_definition(
        &client
            .request("textDocument/definition", definition.clone(), None)?
            .0,
        &uris["dep"],
    )?;
    require_completion(
        &client
            .request("textDocument/completion", completion.clone(), None)?
            .0,
    )?;
    let mut measured = BTreeMap::new();
    measured.insert(
        "hover_warm".to_owned(),
        warm_queries(
            client,
            "textDocument/hover",
            &small,
            samples,
            warmup,
            |result| require_hover(result, SMALL_SIGNATURE, None),
        )?,
    );
    measured.insert(
        "definition_warm".to_owned(),
        warm_queries(
            client,
            "textDocument/definition",
            &definition,
            samples,
            warmup,
            |result| require_definition(result, &uris["dep"]),
        )?,
    );
    measured.insert(
        "completion_warm".to_owned(),
        warm_queries(
            client,
            "textDocument/completion",
            &completion,
            samples,
            warmup,
            require_completion,
        )?,
    );

    measured.insert(
        "open_to_hover_large".to_owned(),
        causal_open(client, workspace, body, samples, warmup)?,
    );

    measured.insert(
        "edit_to_hover_large".to_owned(),
        causal_edit(client, workspace, body, samples, warmup)?,
    );

    measured.insert(
        "hover_during_large_edit".to_owned(),
        busy_edit(client, workspace, &uris["small"], body, samples, warmup)?,
    );

    measured.insert(
        "hover_during_large_open".to_owned(),
        busy_open(client, workspace, &uris["small"], body, samples, warmup)?,
    );
    client.finish()?;
    Ok(measured)
}

pub(crate) fn causal_open(
    client: &mut LspProcess,
    workspace: &Path,
    body: &str,
    samples: usize,
    warmup: usize,
) -> ToolResult<Vec<u64>> {
    let workload = "open_to_hover_large";
    let mut values = Vec::with_capacity(samples);
    for revision in 1..=warmup + samples {
        let uri = file_uri(&workspace.join(format!("{workload}_{revision}.bend")))?;
        let (name, source) = large_revision(body, workload, revision);
        let (result, elapsed) = client.request(
            "textDocument/hover",
            position(&uri, 0, 5),
            Some(open_message(&uri, &source, 1)),
        )?;
        require_hover(&result, &format!("def {name}: U32"), None)?;
        if revision > warmup {
            values.push(elapsed);
        }
        client.wait_diagnostics(&uri, Some(1))?;
        client.close_document(&uri)?;
    }
    Ok(values)
}

pub(crate) fn causal_edit(
    client: &mut LspProcess,
    workspace: &Path,
    body: &str,
    samples: usize,
    warmup: usize,
) -> ToolResult<Vec<u64>> {
    let workload = "edit_to_hover_large";
    let uri = file_uri(&workspace.join(format!("{workload}.bend")))?;
    let (mut previous, source) = large_revision(body, workload, 1);
    send_notification(client, &open_message(&uri, &source, 1))?;
    require_hover(
        &client
            .request("textDocument/hover", position(&uri, 0, 5), None)?
            .0,
        &format!("def {previous}: U32"),
        None,
    )?;
    client.wait_diagnostics(&uri, Some(1))?;
    let mut values = Vec::with_capacity(samples);
    for index in 0..warmup + samples {
        let revision = index + 2;
        let version = i64::try_from(revision)?;
        let (name, source) = large_revision(body, workload, revision);
        let (result, elapsed) = client.request(
            "textDocument/hover",
            position(&uri, 0, 5),
            Some(change_message(&uri, &source, version)),
        )?;
        require_hover(&result, &format!("def {name}: U32"), Some(&previous))?;
        if index >= warmup {
            values.push(elapsed);
        }
        client.wait_diagnostics(&uri, Some(version))?;
        previous = name;
    }
    client.close_document(&uri)?;
    Ok(values)
}

pub(crate) fn busy_edit(
    client: &mut LspProcess,
    workspace: &Path,
    small_uri: &str,
    body: &str,
    samples: usize,
    warmup: usize,
) -> ToolResult<Vec<u64>> {
    let workload = "hover_during_large_edit";
    let uri = file_uri(&workspace.join(format!("{workload}.bend")))?;
    let (mut previous, source) = large_revision(body, workload, 1);
    send_notification(client, &open_message(&uri, &source, 1))?;
    require_hover(
        &client
            .request("textDocument/hover", position(&uri, 0, 5), None)?
            .0,
        &format!("def {previous}: U32"),
        None,
    )?;
    client.wait_diagnostics(&uri, Some(1))?;
    let mut burst = Vec::new();
    for (index, count) in [warmup, samples]
        .into_iter()
        .filter(|count| *count > 0)
        .enumerate()
    {
        let revision = index + 2;
        let version = i64::try_from(revision)?;
        let (name, source) = large_revision(body, workload, revision);
        burst = pipelined_hovers(
            client,
            small_uri,
            count,
            &change_message(&uri, &source, version),
        )?;
        require_hover(
            &client
                .request("textDocument/hover", position(&uri, 0, 5), None)?
                .0,
            &format!("def {name}: U32"),
            Some(&previous),
        )?;
        client.wait_diagnostics(&uri, Some(version))?;
        previous = name;
    }
    client.close_document(&uri)?;
    Ok(burst)
}

pub(crate) fn busy_open(
    client: &mut LspProcess,
    workspace: &Path,
    small_uri: &str,
    body: &str,
    samples: usize,
    warmup: usize,
) -> ToolResult<Vec<u64>> {
    let workload = "hover_during_large_open";
    let mut burst = Vec::new();
    for (index, count) in [warmup, samples]
        .into_iter()
        .filter(|count| *count > 0)
        .enumerate()
    {
        let revision = index + 1;
        let uri = file_uri(&workspace.join(format!("{workload}_{revision}.bend")))?;
        let (name, source) = large_revision(body, workload, revision);
        burst = pipelined_hovers(client, small_uri, count, &open_message(&uri, &source, 1))?;
        require_hover(
            &client
                .request("textDocument/hover", position(&uri, 0, 5), None)?
                .0,
            &format!("def {name}: U32"),
            None,
        )?;
        client.wait_diagnostics(&uri, Some(1))?;
        client.close_document(&uri)?;
    }
    Ok(burst)
}

fn measure_round(
    binary: &Path,
    body: &str,
    samples: usize,
    warmup: usize,
) -> ToolResult<BTreeMap<String, Vec<u64>>> {
    let workspace = tempfile::Builder::new()
        .prefix("bend-lsp-latency-")
        .tempdir()?;
    let workspace_path = workspace.path().canonicalize()?;
    let mut client = LspProcess::spawn(binary, &workspace_path, &[])?;
    match workloads(&mut client, &workspace_path, body, samples, warmup) {
        Ok(result) => Ok(result),
        Err(error) => {
            let stderr = client.stderr_tail()?;
            Err(format!("{error}\nLSP stderr (last 8 KiB):\n{stderr}").into())
        }
    }
}

#[derive(Parser)]
#[command(no_binary_name = true)]
struct CollectArgs {
    #[arg(long)]
    baseline_binary: PathBuf,
    #[arg(long)]
    candidate_binary: PathBuf,
    #[arg(long)]
    baseline_output: PathBuf,
    #[arg(long)]
    candidate_output: PathBuf,
    #[arg(long, default_value = "7", value_parser = positive)]
    rounds: usize,
    #[arg(long, default_value = "32", value_parser = positive)]
    samples: usize,
    #[arg(long, default_value = "8")]
    warmup: usize,
}

fn positive(value: &str) -> Result<usize, String> {
    let number = value.parse::<usize>().map_err(|error| error.to_string())?;
    if number == 0 {
        return Err("must be positive".into());
    }
    Ok(number)
}

fn output_path(path: &Path) -> ToolResult<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let name = path.file_name().ok_or("output must name a file")?;
    let parent = path.parent().ok_or("output has no parent")?;
    fs::create_dir_all(parent)?;
    Ok(parent.canonicalize()?.join(name))
}

fn publish_pair(outputs: &[PathBuf; 2], measurements: &[Value; 2]) -> ToolResult<()> {
    let mut staged = Vec::with_capacity(2);
    for (output, measurement) in outputs.iter().zip(measurements) {
        let parent = output.parent().ok_or("output has no parent")?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer_pretty(&mut file, measurement)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        staged.push(file);
    }
    for (file, output) in staged.into_iter().zip(outputs) {
        file.persist(output)?;
    }
    Ok(())
}

struct CollectionInputs {
    fixture: PathBuf,
    binaries: [PathBuf; 2],
    outputs: [PathBuf; 2],
    harness_binary: PathBuf,
}

fn collection_inputs(args: &CollectArgs) -> ToolResult<CollectionInputs> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../benches/fixtures/analyzer_large.bend")
        .canonicalize()?;
    let binaries = [
        args.baseline_binary.canonicalize()?,
        args.candidate_binary.canonicalize()?,
    ];
    let outputs = [
        output_path(&args.baseline_output)?,
        output_path(&args.candidate_output)?,
    ];
    if outputs[0] == outputs[1] {
        return Err("baseline and candidate output paths must differ".into());
    }
    let sources = [
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/latency.rs"),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/transport.rs"),
    ];
    let harness_binary = std::env::current_exe()?.canonicalize()?;
    for output in &outputs {
        let resolved = if output.exists() {
            output.canonicalize()?
        } else {
            output.clone()
        };
        if binaries.contains(&resolved)
            || resolved == fixture
            || resolved == harness_binary
            || sources.contains(&resolved)
        {
            return Err("output paths must not overwrite binaries, harness, or fixture".into());
        }
    }
    for binary in &binaries {
        if !binary.is_file() {
            return Err(format!("binary is not a file: {}", binary.display()).into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(binary)?.permissions().mode() & 0o111 == 0 {
                return Err(format!("binary is not executable: {}", binary.display()).into());
            }
        }
    }
    Ok(CollectionInputs {
        fixture,
        binaries,
        outputs,
        harness_binary,
    })
}

fn collect_rounds(
    args: &CollectArgs,
    binaries: &[PathBuf; 2],
    body: &str,
    order: &[[&str; 2]],
    measurements: &mut [Value; 2],
    outputs: &[PathBuf; 2],
) -> ToolResult<()> {
    for (round, variants) in order.iter().enumerate() {
        for variant in variants {
            let index = usize::from(*variant == "candidate");
            eprintln!("round {}/{}: {variant}", round + 1, args.rounds);
            let result = measure_round(&binaries[index], body, args.samples, args.warmup)
                .map_err(|error| format!("round {} {variant}: {error}", round + 1))?;
            if result.len() != WORKLOADS.len()
                || WORKLOADS.iter().any(|name| {
                    !result.get(*name).is_some_and(|values| {
                        values.len() == args.samples && values.iter().all(|value| *value > 0)
                    })
                })
            {
                return Err("round did not produce every required positive sample".into());
            }
            for name in WORKLOADS {
                measurements[index]["workloads"][name]["rounds_ns"]
                    .as_array_mut()
                    .ok_or("missing round samples")?
                    .push(json!(result[name]));
            }
            publish_pair(outputs, measurements)?;
        }
    }
    Ok(())
}

pub(crate) fn collect(args: &[String]) -> ToolResult<()> {
    let args = CollectArgs::try_parse_from(args)?;
    common::require_ci()?;
    let target = common::native_target()?;
    args.samples
        .checked_add(args.warmup)
        .and_then(|total| total.checked_add(2))
        .ok_or("sample count overflow")?;
    let CollectionInputs {
        fixture,
        binaries,
        outputs,
        harness_binary,
    } = collection_inputs(&args)?;
    let fixture_bytes = fs::read(&fixture)?;
    if fixture_bytes.is_empty() {
        return Err("large fixture is empty".into());
    }
    let fixture_text = std::str::from_utf8(&fixture_bytes)?;
    let separator = if fixture_text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let body = format!("{fixture_text}{separator}").repeat(FIXTURE_REPETITIONS);
    let mut harness = Sha256::new();
    harness.update(include_bytes!("transport.rs"));
    harness.update(include_bytes!("latency.rs"));
    let harness_sha256 = format!("{:x}", harness.finalize());
    let fixture_sha256 = format!("{:x}", Sha256::digest(&fixture_bytes));
    let digest_input = json!({"format_version":1,"harness_sha256":harness_sha256,"fixture_sha256":fixture_sha256,"fixture_repetitions":FIXTURE_REPETITIONS,"workloads":WORKLOADS,"rounds":args.rounds,"samples":args.samples,"warmup":args.warmup});
    let workload_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&digest_input)?));
    let hashes = [
        common::sha256_file(&binaries[0])?,
        common::sha256_file(&binaries[1])?,
    ];
    let harness_binary_sha256 = common::sha256_file(&harness_binary)?;
    let order: Vec<[&str; 2]> = (0..args.rounds)
        .map(|index| {
            if index % 2 == 0 {
                ["baseline", "candidate"]
            } else {
                ["candidate", "baseline"]
            }
        })
        .collect();
    let mut metadata = json!({
        "platform":std::env::consts::OS,"machine":std::env::consts::ARCH,"target":target,
        "harness":concat!("bend2-perf/",env!("CARGO_PKG_VERSION")),"harness_binary_sha256":harness_binary_sha256,
        "rounds":args.rounds,"samples":args.samples,"warmup":args.warmup,"harness_sha256":harness_sha256,
        "fixture_sha256":fixture_sha256,"fixture_bytes":fixture_bytes.len(),"fixture_repetitions":FIXTURE_REPETITIONS,
        "large_body_bytes":body.len(),"generated_adt_source":ADT_SOURCE,"round_order":order,
        "compiler_configuration":{"compilerPath":"<fresh-workspace>/unavailable-compiler/bend (does not exist)","compilerArguments":[],"real_compiler_execution":false,
            "PATH":"<fresh-workspace>/empty-path","HOME":"<fresh-workspace>/empty-home","USERPROFILE":"<fresh-workspace>/empty-home","removed_environment":["BEND*"],
            "readiness":"semantic response plus compiler-unavailable diagnostics for exact revision"},
        "timing":{"clock":"std::time::Instant","start":"before frame write; causal open/edit include notification frame write",
            "end":"immediately after full response body read, before JSON parsing","busy_requests":"per-request hover latency; large notification precedes pipelined burst",
            "initialization_measured":false,"request_timeout_seconds":REQUEST_TIMEOUT.as_secs()}
    });
    metadata["collection_status"] = json!("running");
    let mut measurements: [Value; 2] = std::array::from_fn(|index| {
        let mut metadata = metadata.clone();
        metadata["binary_sha256"] = json!(hashes[index]);
        let workloads: serde_json::Map<String, Value> = WORKLOADS
            .into_iter()
            .map(|name| (name.to_owned(), json!({"rounds_ns":[]})))
            .collect();
        json!({"format_version":1,"workload_digest":workload_digest,"metadata":metadata,"workloads":workloads})
    });
    publish_pair(&outputs, &measurements)?;
    let result = (|| -> ToolResult<()> {
        collect_rounds(&args, &binaries, &body, &order, &mut measurements, &outputs)?;
        if [
            common::sha256_file(&binaries[0])?,
            common::sha256_file(&binaries[1])?,
        ] != hashes
        {
            return Err("a measured binary changed during collection".into());
        }
        if common::sha256_file(&fixture)? != fixture_sha256
            || common::sha256_file(&harness_binary)? != harness_binary_sha256
        {
            return Err("harness or fixture changed during collection".into());
        }
        Ok(())
    })();
    for measurement in &mut measurements {
        measurement["metadata"]["collection_status"] =
            json!(if result.is_ok() { "complete" } else { "failed" });
        if let Err(error) = &result {
            measurement["metadata"]["collection_error"] = json!(error.to_string());
        }
    }
    publish_pair(&outputs, &measurements)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hover_requires_exact_revision_and_known_signature() -> ToolResult<()> {
        let current = json!({"contents":{"kind":"markdown","value":"def current_revision: U32"}});
        require_hover(&current, "def current_revision: U32", Some("old_revision"))?;
        assert!(require_hover(&current, "def missing_revision: U32", None).is_err());
        let stale =
            json!({"contents":{"value":"def current_revision: U32; def old_revision: U32"}});
        assert!(require_hover(&stale, "def current_revision: U32", Some("old_revision")).is_err());
        assert!(require_hover(&Value::Null, SMALL_SIGNATURE, None).is_err());
        Ok(())
    }

    #[test]
    fn definitions_and_completion_must_be_semantically_correct() -> ToolResult<()> {
        let uri = "file:///dependency.bend";
        let exact = json!({"uri":uri,"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":9}}});
        require_definition(&exact, uri)?;
        assert!(require_definition(&exact, "file:///other.bend").is_err());
        let mut wrong_span = exact;
        wrong_span["range"]["end"]["character"] = json!(10);
        assert!(require_definition(&wrong_span, uri).is_err());
        require_completion(&json!([{"label":"add"}]))?;
        require_completion(&json!({"items":[{"label":"add"}]}))?;
        assert!(require_completion(&json!({"items":[{"label":"subtract"}]})).is_err());
        assert!(require_completion(&Value::Null).is_err());
        Ok(())
    }
}
