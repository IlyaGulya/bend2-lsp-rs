use crate::{
    ToolResult,
    callgrind::{
        self, BENCHMARKS, Identity, METRICS, baseline_name, identity, parse_measurement,
        parse_workload_list, require, sha256_bytes, strict_json,
    },
    policy::{self, PROVENANCE_FIELDS},
    repeated,
};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
};

fn text<'a>(value: &'a Value, key: &str) -> ToolResult<&'a str> {
    value[key]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing or empty {key}").into())
}

fn hex(value: &Value, length: usize) -> bool {
    value.as_str().is_some_and(|value| {
        value.len() == length
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

// Executable/profile names describe the Linux producer, not the replay host.
fn recorded_absolute(name: &str) -> bool {
    name.strip_prefix('/').is_some_and(|relative| {
        !relative.contains('\0')
            && relative
                .split('/')
                .all(|component| !component.is_empty() && component != "." && component != "..")
    })
}

fn relative(name: &str) -> ToolResult<&Path> {
    let path = Path::new(name);
    require(
        !name.is_empty()
            && path
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            && !name.contains('\\')
            && path.to_str() == Some(name),
        "evidence path must be a normalized relative path",
    )?;
    require(
        name.split('/')
            .all(|component| !component.is_empty() && component != "." && component != ".."),
        "evidence path must be normalized",
    )?;
    Ok(path)
}

fn profile_counts(text: &str) -> ToolResult<[u64; METRICS.len()]> {
    require(
        text.starts_with("# callgrind format"),
        "missing or unsupported raw Callgrind profile",
    )?;
    let mut columns = [None; METRICS.len()];
    let mut events = None;
    let mut summary = None;
    let mut totals = None;
    for line in text.lines() {
        require(
            !line.contains(callgrind::PROBE),
            "layout probe appears in collected Callgrind execution; the layout control is invalid",
        )?;
        if let Some(header) = line.strip_prefix("events:") {
            require(events.is_none(), "duplicate Callgrind events header")?;
            let mut width = 0;
            for (column, event) in header.split_ascii_whitespace().enumerate() {
                if let Some(metric) = METRICS.iter().position(|metric| *metric == event) {
                    require(
                        columns[metric].replace(column).is_none(),
                        "duplicate required Callgrind event",
                    )?;
                }
                width += 1;
            }
            require(
                columns.iter().all(Option::is_some),
                "missing required Callgrind event",
            )?;
            events = Some(width);
        } else if let Some((values, target)) = line
            .strip_prefix("totals:")
            .map(|values| (values, &mut totals))
            .or_else(|| {
                line.strip_prefix("summary:")
                    .map(|values| (values, &mut summary))
            })
        {
            let width = events.ok_or("Callgrind counts precede events")?;
            require(target.is_none(), "duplicate Callgrind counter record")?;
            let mut counts = [0; METRICS.len()];
            let mut length = 0;
            for (column, value) in values.split_ascii_whitespace().enumerate() {
                require(
                    value.bytes().all(|byte| byte.is_ascii_digit()),
                    "invalid Callgrind counter",
                )?;
                let count = value.parse::<u64>()?;
                if let Some(metric) = columns.iter().position(|index| *index == Some(column)) {
                    counts[metric] = count;
                }
                length += 1;
            }
            // Callgrind omits trailing zero counters, including required cache events.
            require(
                length > 0 && length <= width,
                "Callgrind counter columns differ from events",
            )?;
            *target = Some(counts);
        }
    }
    // Iai 0.16.1's SummaryParser prefers totals: client requests can make summary wrong.
    // Keep scanning after totals to reject contradictory records and embedded layout probes.
    totals
        .or(summary)
        .ok_or_else(|| "missing Callgrind summary or totals".into())
}

struct Inputs {
    root: PathBuf,
    files: BTreeMap<String, Value>,
    #[cfg(unix)]
    inodes: BTreeSet<(u64, u64)>,
}

impl Inputs {
    fn new(root: &Path) -> ToolResult<Self> {
        Ok(Self {
            root: root.canonicalize()?,
            files: BTreeMap::new(),
            #[cfg(unix)]
            inodes: BTreeSet::new(),
        })
    }

    fn read(&mut self, name: &str) -> ToolResult<String> {
        let path = self.root.join(relative(name)?);
        require(
            !self.files.contains_key(name),
            "aliased paired evidence input",
        )?;
        require(
            path.canonicalize()? == path && path.is_file(),
            "evidence must be a confined regular non-symlink file",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = fs::metadata(&path)?;
            require(
                self.inodes.insert((metadata.dev(), metadata.ino())),
                "aliased paired evidence inode",
            )?;
        }
        let bytes = fs::read(path)?;
        self.files.insert(
            name.to_owned(),
            json!({"path": name, "sha256": sha256_bytes(&bytes), "size": bytes.len()}),
        );
        Ok(String::from_utf8(bytes)?)
    }

    fn profile(&mut self, emitted: &str, home: &str) -> ToolResult<[u64; METRICS.len()]> {
        require(
            recorded_absolute(emitted),
            "Callgrind profile must retain its original absolute Linux path",
        )?;
        let marker = format!("/{home}/");
        let (_, suffix) = emitted
            .rsplit_once(&marker)
            .ok_or("profile escaped its fresh benchmark home")?;
        let name = format!("{home}/{suffix}");
        profile_counts(&self.read(&name)?)
    }
}

pub(super) fn metadata(data: &Value, manifest: &Value) -> ToolResult<()> {
    require(
        data["format_version"] == 1
            && data["kind"] == "paired-callgrind"
            && data["method"] == repeated::METHOD
            && data["pairs"] == repeated::PAIRS,
        "unsupported paired dataset or sample count",
    )?;
    require(
        manifest["schema_version"] == 1
            && manifest["mode"] == "callgrind"
            && manifest["measurement_method"] == repeated::METHOD,
        "paired evidence requires its canonical artifact envelope",
    )?;
    text(data, "job_id")?;
    require(
        data["provenance"]
            .as_object()
            .is_some_and(|fields| fields.len() == PROVENANCE_FIELDS.len()),
        "invalid paired provenance",
    )?;
    for field in PROVENANCE_FIELDS {
        text(manifest, field)?;
        if data["provenance"][field] != manifest[field] {
            return Err(format!("paired {field} differs from source manifest").into());
        }
    }
    for field in ["base_sha", "candidate_sha", "workflow_sha"] {
        require(hex(&manifest[field], 40), "invalid source revision")?;
    }
    for field in ["run_id", "run_attempt"] {
        require(
            text(manifest, field)?
                .parse::<u64>()
                .is_ok_and(|value| value > 0),
            "invalid source run identity",
        )?;
    }
    let gate = &manifest["gate_identity"];
    require(
        data["gate_identity"] == *gate
            && gate["workflow"] == "performance"
            && gate["job"] == "compare"
            && gate["step"] == "Compare pull request with main"
            && hex(&gate["head_sha"], 40),
        "invalid paired gate identity",
    )?;
    let environment = &data["environment"];
    for field in ["rustc", "cargo", "iai_runner", "valgrind", "os", "arch"] {
        text(environment, field)?;
    }
    require(
        text(environment, "rustc")?.starts_with("rustc 1.98.1 ")
            || environment["rustc"] == "rustc 1.98.1",
        "paired evidence requires pinned Rust",
    )?;
    require(
        environment["iai_runner"] == "iai-callgrind-runner 0.16.1"
            && environment["valgrind"] == "valgrind-3.27.1"
            && environment["cache_args"] == json!(["--cache-sim=yes"]),
        "paired evidence requires pinned collection tools and cache arguments",
    )?;
    require(
        data["host"]
            .as_object()
            .is_some_and(|host| !host.is_empty()),
        "paired evidence requires host metadata",
    )
}

fn ids(rows: &Value) -> ToolResult<BTreeSet<Identity>> {
    let rows = rows
        .as_array()
        .filter(|rows| !rows.is_empty())
        .ok_or("missing workload inventory")?;
    let mut result = BTreeSet::new();
    for row in rows {
        require(result.insert(identity(row)?), "duplicate workload identity")?;
    }
    Ok(result)
}

fn benchmarks(value: &Value) -> ToolResult<&Map<String, Value>> {
    let map = value.as_object().ok_or("missing benchmark inventory")?;
    require(
        map.len() == BENCHMARKS.len() && BENCHMARKS.iter().all(|name| map.contains_key(*name)),
        "both original benchmark executables are required",
    )?;
    Ok(map)
}

fn source(inputs: &mut Inputs, variant: &Value) -> ToolResult<()> {
    let document = strict_json(&inputs.read(text(variant, "source_manifest")?)?)?;
    let files = document["files"]
        .as_array()
        .filter(|files| !files.is_empty())
        .ok_or("missing immutable source inventory")?;
    let mut previous = None;
    for file in files {
        let name = text(file, "path")?;
        relative(name)?;
        require(
            previous.is_none_or(|previous| previous < name)
                && hex(&file["sha256"], 64)
                && file["executable"].is_boolean(),
            "invalid or duplicate source manifest file",
        )?;
        previous = Some(name);
    }
    require(
        hex(&variant["source_sha256"], 64)
            && document["sha256"] == variant["source_sha256"]
            && sha256_bytes(&serde_json::to_vec(files)?) == text(&document, "sha256")?,
        "source manifest digest differs from immutable source identity",
    )
}

type Inventories = BTreeMap<String, BTreeMap<String, BTreeSet<Identity>>>;

fn variants(data: &Value, manifest: &Value, inputs: &mut Inputs) -> ToolResult<Inventories> {
    let variants = data["variants"]
        .as_object()
        .ok_or("missing paired variants")?;
    require(
        variants.len() == 2 && variants.contains_key("a") && variants.contains_key("b"),
        "paired variants must be exactly a and b",
    )?;
    let mut inventories = BTreeMap::new();
    let mut executables = BTreeSet::new();
    for (name, revision) in [("a", "base_sha"), ("b", "candidate_sha")] {
        let variant = &variants[name];
        require(
            variant["source_revision"] == manifest[revision],
            "variant revision differs from source provenance",
        )?;
        source(inputs, variant)?;
        let mut owned = BTreeSet::new();
        let mut inventory = BTreeMap::new();
        for (benchmark, metadata) in benchmarks(&variant["benchmarks"])? {
            let executable = text(metadata, "executable")?;
            require(
                recorded_absolute(executable)
                    && executables.insert(executable)
                    && hex(&metadata["binary_sha256"], 64),
                "invalid or reused benchmark executable identity",
            )?;
            let expected = ids(&metadata["workloads"])?;
            let actual =
                parse_workload_list(&inputs.read(text(metadata, "workload_list")?)?, benchmark)?;
            require(
                expected == actual,
                "declared workload inventory differs from executed --list",
            )?;
            for id in &expected {
                require(
                    owned.insert(id.clone()),
                    "workload belongs to multiple benchmarks",
                )?;
            }
            inventory.insert(benchmark.clone(), expected);
        }
        inventories.insert(name.to_owned(), inventory);
    }
    for benchmark in BENCHMARKS {
        require(
            inventories["a"][benchmark].is_subset(&inventories["b"][benchmark]),
            "candidate is missing or reassigning a baseline workload",
        )?;
    }
    Ok(inventories)
}

fn counts(rows: &[Value]) -> ToolResult<BTreeMap<Identity, &Value>> {
    let mut result = BTreeMap::new();
    for row in rows {
        let values = row["counts"].as_object().ok_or("missing workload counts")?;
        require(
            values.len() == METRICS.len()
                && METRICS.iter().all(|metric| {
                    values
                        .get(*metric)
                        .is_some_and(|value| value.as_u64().is_some())
                }),
            "workload counts must be exactly unsigned Ir, I1mr, ILmr",
        )?;
        require(
            result.insert(identity(row)?, &row["counts"]).is_none(),
            "duplicate measured workload",
        )?;
    }
    Ok(result)
}

fn sample(
    data: &Value,
    sample: &Value,
    inventory: &BTreeMap<String, BTreeSet<Identity>>,
    inputs: &mut Inputs,
) -> ToolResult<Vec<Value>> {
    let variant = text(sample, "variant")?;
    let pair = sample["pair"].as_u64().ok_or("missing sample pair")?;
    let label = baseline_name(text(data, "job_id")?, pair, variant);
    let evidence = benchmarks(&sample["benchmarks"])?;
    let mut measured = Vec::new();
    for benchmark in BENCHMARKS {
        let evidence = &evidence[benchmark];
        let directory = format!("raw/pair{pair:03}{variant}/{benchmark}");
        let stdout = text(evidence, "raw_stdout")?;
        let stderr = text(evidence, "raw_stderr")?;
        require(
            Path::new(stdout).parent() == Some(Path::new(&directory))
                && Path::new(stderr).parent() == Some(Path::new(&directory)),
            "raw evidence must belong to its fresh pair and benchmark",
        )?;
        let output = inputs.read(stdout)?;
        inputs.read(stderr)?;
        let executable = text(
            &data["variants"][variant]["benchmarks"][benchmark],
            "executable",
        )?;
        let (rows, summaries) = parse_measurement(
            &output,
            &label,
            Path::new(executable),
            &inventory[benchmark],
        )?;
        let mut profiles = 0_u64;
        for (summary, row) in summaries.iter().zip(&rows) {
            let paths = summary["profiles"][0]["out_paths"]
                .as_array()
                .ok_or("missing retained Callgrind profiles")?;
            // Iai emits one part per parsed file; parse_measurement requires one fresh part.
            require(
                paths.len() == 1,
                "one fresh Callgrind part requires one retained profile",
            )?;
            let retained = inputs.profile(
                paths[0].as_str().ok_or("invalid Callgrind profile path")?,
                &format!("{directory}/iai"),
            )?;
            for (metric, count) in METRICS.iter().zip(retained) {
                require(
                    row["counts"][*metric].as_u64() == Some(count),
                    "retained Callgrind counters contradict raw stdout",
                )?;
            }
            profiles += 1;
        }
        require(
            evidence["verified_callgrind_profiles"].as_u64() == Some(profiles),
            "verified profile count differs from retained execution",
        )?;
        measured.extend(rows);
    }
    let declared = sample["workloads"]
        .as_array()
        .ok_or("missing sample workloads")?;
    require(
        counts(declared)? == counts(&measured)?,
        "dataset counts contradict retained raw stdout",
    )?;
    Ok(measured)
}

fn observations(
    data: &Value,
    inventories: &Inventories,
    inputs: &mut Inputs,
) -> ToolResult<Vec<Value>> {
    let samples = data["samples"].as_array().ok_or("missing paired samples")?;
    require(
        samples.len() == 2 * repeated::PAIRS,
        "paired evidence requires exactly 105 complete pairs",
    )?;
    let mut measured = BTreeMap::new();
    for row in samples {
        let pair = row["pair"]
            .as_u64()
            .filter(|pair| (1..=repeated::PAIRS as u64).contains(pair))
            .ok_or("invalid paired sample number")?;
        let variant = text(row, "variant")?;
        require(
            matches!(variant, "a" | "b"),
            "invalid paired sample variant",
        )?;
        let order = u64::from((pair % 2 == 1) != (variant == "a"));
        require(
            row["order"].as_u64() == Some(order),
            "pair execution order does not alternate",
        )?;
        require(
            !measured.contains_key(&(pair, variant)),
            "duplicate paired sample",
        )?;
        measured.insert(
            (pair, variant),
            sample(data, row, &inventories[variant], inputs)?,
        );
    }
    let mut raw = Vec::new();
    for pair in 1..=repeated::PAIRS as u64 {
        let baseline = counts(
            measured
                .get(&(pair, "a"))
                .ok_or("missing baseline sample")?,
        )?;
        let candidate = counts(
            measured
                .get(&(pair, "b"))
                .ok_or("missing candidate sample")?,
        )?;
        for (id, values) in baseline {
            let current = candidate.get(&id).ok_or("missing candidate workload")?;
            for metric in METRICS {
                let baseline = values[metric].as_u64().ok_or("invalid baseline count")?;
                let candidate = current[metric].as_u64().ok_or("invalid candidate count")?;
                require(
                    metric != "Ir" || baseline == 0 || candidate != 0,
                    "empty candidate instruction collection cannot pass a nonzero baseline",
                )?;
                raw.push(
                    json!({"job_id": data["job_id"], "role": "measurement", "variant": "b",
                    "function_name": id.0, "id": id.1, "metric": metric,
                    "baseline": baseline, "candidate": candidate,
                    "current_passed": policy::within_limit(metric, baseline, candidate)?}),
                );
            }
        }
    }
    Ok(raw)
}

pub(crate) fn build(root: &Path, manifest: &Value) -> ToolResult<Value> {
    let mut inputs = Inputs::new(root)?;
    let data = strict_json(&inputs.read("paired-data.json")?)?;
    metadata(&data, manifest)?;
    let inventories = variants(&data, manifest, &mut inputs)?;
    let raw = observations(&data, &inventories, &mut inputs)?;
    let mut workloads: BTreeMap<Identity, Map<String, Value>> = BTreeMap::new();
    let mut failures = Vec::new();
    let mut status = "complete";
    for comparison in repeated::comparisons(&raw)? {
        let id = identity(&comparison)?;
        let metric = text(&comparison, "metric")?.to_owned();
        let classification = text(&comparison, "classification")?;
        match classification {
            "passed" => {}
            "regression" => {
                if status != "inconclusive" {
                    status = "regression";
                }
            }
            "inconclusive" => status = "inconclusive",
            _ => return Err("qualified estimator returned incomplete paired evidence".into()),
        }
        if classification != "passed" {
            failures.push(json!({"function_name": id.0, "id": id.1, "metric": metric, "classification": classification}));
        }
        workloads.entry(id).or_default().insert(metric, comparison);
    }
    let workloads: Vec<_> = workloads
        .into_iter()
        .map(|(id, metrics)| json!({"function_name": id.0, "id": id.1, "metrics": metrics}))
        .collect();
    let passed = workloads
        .iter()
        .filter(|row| {
            METRICS
                .iter()
                .all(|metric| row["metrics"][metric]["classification"] == "passed")
        })
        .count();
    let mut new_workloads = Vec::new();
    for benchmark in BENCHMARKS {
        for id in inventories["b"][benchmark].difference(&inventories["a"][benchmark]) {
            new_workloads.push(json!({"function_name": id.0, "id": id.1, "benchmark": benchmark}));
        }
    }
    let dataset = inputs
        .files
        .remove("paired-data.json")
        .ok_or("missing dataset identity")?;
    Ok(
        json!({"format_version": 2, "kind": "callgrind-policy", "method": repeated::METHOD,
        "status": status, "passed": status == "complete", "provenance": data["provenance"], "gate_identity": data["gate_identity"],
        "inputs": {"dataset": dataset, "files": inputs.files.into_values().collect::<Vec<_>>()},
        "workload_count": workloads.len(), "passed_count": passed, "workloads": workloads,
        "new_workloads": new_workloads, "failures": failures}),
    )
}

fn fraction(value: &Value) -> ToolResult<String> {
    let numerator = value["numerator"]
        .as_number()
        .ok_or("missing residual numerator")?;
    let denominator = value["denominator"]
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or("missing residual denominator")?;
    Ok(format!("{numerator}/{denominator}"))
}

fn escaped(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}

pub(crate) fn markdown(verdict: &Value) -> ToolResult<String> {
    let mut output = format!(
        "# Paired Callgrind admission\n\nMethod: `{}`. Status: **{}**. Each comparison uses {} fresh-process pairs on one host. Bounds are normalized allowance-adjusted residuals, not editor latency or synthetic count pairs.\n\n| Workload | Event | Samples | Median residual | Lower bound | Upper bound | Classification |\n| --- | --- | ---: | --- | --- | --- | --- |\n",
        text(verdict, "method")?,
        text(verdict, "status")?,
        repeated::PAIRS
    );
    for workload in verdict["workloads"]
        .as_array()
        .ok_or("missing verdict workloads")?
    {
        let id = identity(workload)?;
        let label = escaped(&format!(
            "{} [{}]",
            id.0,
            id.1.as_deref().unwrap_or("default")
        ));
        for metric in METRICS {
            let comparison = &workload["metrics"][metric];
            writeln!(
                output,
                "| {label} | {metric} | {} | {} | {} | {} | {} |",
                comparison["samples"],
                fraction(&comparison["median"])?,
                fraction(&comparison["lower"])?,
                fraction(&comparison["upper"])?,
                text(comparison, "classification")?
            )?;
        }
    }
    output.push_str("\n## New candidate workloads\n\nThese workloads have no baseline comparison and do not substitute for any baseline workload.\n");
    for workload in verdict["new_workloads"]
        .as_array()
        .ok_or("missing new workloads")?
    {
        let id = identity(workload)?;
        writeln!(
            output,
            "- {} [{}]",
            escaped(&id.0),
            escaped(id.1.as_deref().unwrap_or("default"))
        )?;
    }
    Ok(output)
}
pub(crate) fn validate(root: &Path, manifest: &Value, verdict: &Value) -> ToolResult<Vec<Value>> {
    let mut authoritative = build(root, manifest)?;
    require(
        authoritative == *verdict,
        "paired verdict differs from recomputed raw evidence",
    )?;
    let mut inventory = BTreeMap::new();
    for entry in manifest["files"]
        .as_array()
        .ok_or("missing artifact inventory")?
    {
        let name = text(entry, "path")?;
        require(
            inventory.insert(name, entry).is_none(),
            "duplicate artifact inventory identity",
        )?;
    }
    let files = authoritative["inputs"]["files"]
        .as_array()
        .ok_or("missing paired input inventory")?;
    for input in std::iter::once(&authoritative["inputs"]["dataset"]).chain(files) {
        let entry = inventory
            .get(text(input, "path")?)
            .ok_or("paired input absent from outer inventory")?;
        require(
            entry["sha256"] == input["sha256"] && entry["size"] == input["size"],
            "paired input identity differs from outer manifest",
        )?;
    }
    match authoritative["failures"].take() {
        Value::Array(failures) => Ok(failures),
        _ => Err("missing authoritative paired failures".into()),
    }
}
