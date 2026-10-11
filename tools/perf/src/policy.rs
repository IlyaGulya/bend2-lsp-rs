use crate::ToolResult;
use clap::Parser;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
};

pub(crate) const METRICS: [&str; 3] = ["Ir", "I1mr", "ILmr"];
type BenchmarkId = (String, Option<String>);
type Counts = [u64; 3];
type Pairs = [(u64, u64); 3];

#[derive(Debug)]
pub(crate) struct ExitError {
    pub(crate) status: u8,
    message: String,
}

impl fmt::Display for ExitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ExitError {}

#[derive(Parser)]
#[command(
    name = "bend2-perf policy",
    no_binary_name = true,
    about = "Generate baseline manifests and compare Iai summaries under active policy."
)]
struct Args {
    /// Root containing Iai summary.json files.
    summary_root: PathBuf,
    /// Write the unique benchmark IDs found in a base-run summary tree.
    #[arg(
        long,
        conflicts_with = "baseline_manifest",
        required_unless_present = "baseline_manifest"
    )]
    write_baseline_manifest: Option<PathBuf>,
    /// Authoritative benchmark IDs emitted by the base run.
    #[arg(long, required_unless_present = "write_baseline_manifest")]
    baseline_manifest: Option<PathBuf>,
    /// Name used by Iai's --baseline option when comparing candidate summaries.
    #[arg(long)]
    baseline_name: Option<String>,
    /// Retain the authoritative comparison verdict and exact input identities.
    #[arg(
        long,
        requires = "artifact_manifest",
        conflicts_with = "write_baseline_manifest"
    )]
    verdict: Option<PathBuf>,
    /// Prepared artifact manifest binding the verdict to the hosted source pair.
    #[arg(long, requires = "verdict")]
    artifact_manifest: Option<PathBuf>,
}

#[derive(Debug)]
struct WorkloadResult {
    name: String,
    baseline: Counts,
    candidate: Counts,
    passed: bool,
    id: BenchmarkId,
    summary_path: PathBuf,
    failed_metrics: Vec<&'static str>,
}

struct Comparison {
    results: Vec<WorkloadResult>,
    new_ids: Vec<BenchmarkId>,
    inputs: BTreeMap<BenchmarkId, (Option<Pairs>, PathBuf)>,
}

pub(crate) fn active_allowance(metric: &str, baseline: u64) -> ToolResult<u64> {
    let baseline = u128::from(baseline);
    let allowance = match metric {
        "Ir" => baseline * 2 / 100,
        "I1mr" | "ILmr" => (baseline * 3).div_ceil(100).max(3),
        _ => return Err(format!("unsupported Callgrind metric: {metric}").into()),
    };
    Ok(u64::try_from(allowance)?)
}

pub(crate) fn within_limit(metric: &str, baseline: u64, candidate: u64) -> ToolResult<bool> {
    Ok(u128::from(candidate)
        <= u128::from(baseline) + u128::from(active_allowance(metric, baseline)?))
}

fn canonical_id(summary: &Value) -> ToolResult<BenchmarkId> {
    let object = summary
        .as_object()
        .ok_or("benchmark summary must be an object")?;
    let function_name = object
        .get("function_name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or("benchmark function_name must be a non-empty string")?;
    let id = match object.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) if !id.is_empty() => Some(id.clone()),
        _ => return Err("benchmark id must be null or a non-empty string".into()),
    };
    Ok((function_name.to_owned(), id))
}

fn display_id(id: &BenchmarkId) -> String {
    match &id.1 {
        Some(workload) => format!("{} [{workload}]", id.0),
        None => id.0.clone(),
    }
}

fn load_json(path: &Path) -> ToolResult<Value> {
    Ok(serde_json::from_reader(fs::File::open(path)?)?)
}

// Sort full paths, just as Path.rglob followed by sorted does in the old tool.
fn summary_paths(root: &Path) -> ToolResult<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if entry.file_name() == "summary.json" {
                paths.push(entry.path());
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn load_baseline_manifest(path: &Path) -> ToolResult<BTreeSet<BenchmarkId>> {
    let manifest = load_json(path)?;
    if manifest.get("format_version").and_then(Value::as_u64) != Some(1) {
        return Err("baseline manifest must use format_version 1 and list benchmarks".into());
    }
    let entries = manifest
        .get("benchmarks")
        .and_then(Value::as_array)
        .ok_or("baseline manifest must use format_version 1 and list benchmarks")?;
    let mut ids = BTreeSet::new();
    for entry in entries {
        let id = canonical_id(entry)?;
        if ids.contains(&id) {
            return Err(format!("duplicate baseline benchmark ID {}", display_id(&id)).into());
        }
        ids.insert(id);
    }
    if ids.is_empty() {
        return Err("baseline manifest contains no benchmarks".into());
    }
    Ok(ids)
}

fn write_baseline_manifest(root: &Path, path: &Path) -> ToolResult<usize> {
    let mut ids = BTreeSet::new();
    for summary_path in summary_paths(root)? {
        let id = canonical_id(&load_json(&summary_path)?)?;
        if ids.contains(&id) {
            return Err(format!(
                "duplicate baseline benchmark ID {} in {}",
                display_id(&id),
                summary_path.display()
            )
            .into());
        }
        ids.insert(id);
    }
    if ids.is_empty() {
        return Err(format!("no Iai summary.json files found under {}", root.display()).into());
    }
    let entries: Vec<Value> = ids
        .iter()
        .map(|id| json!({"function_name": id.0, "id": id.1}))
        .collect();
    crate::common::write_json(path, &json!({"format_version": 1, "benchmarks": entries}))?;
    Ok(ids.len())
}

fn has_selected_baseline(summary: &Value, baseline_name: &str) -> ToolResult<bool> {
    if !summary.is_object() {
        return Err("benchmark summary must be an object".into());
    }
    Ok(summary
        .get("baselines")
        .and_then(Value::as_array)
        .is_some_and(|baselines| {
            baselines.len() == 2
                && baselines[0].is_null()
                && baselines[1].as_str() == Some(baseline_name)
        }))
}

fn callgrind_metrics(summary: &Value) -> ToolResult<&Value> {
    summary
        .pointer("/profiles/0/summaries/parts/0/metrics_summary/Callgrind")
        .ok_or_else(|| "missing Callgrind metrics summary".into())
}

fn integer_count(value: &Value) -> Option<u64> {
    value.as_object()?.get("Int")?.as_u64()
}

fn paired_counts(metrics: &Value) -> ToolResult<Option<Pairs>> {
    let object = metrics
        .as_object()
        .ok_or("Callgrind metrics are not an object")?;
    let metric_values = |metric: &str| -> ToolResult<&serde_json::Map<String, Value>> {
        let data = object
            .get(metric)
            .and_then(Value::as_object)
            .ok_or_else(|| format!("missing required Callgrind metric {metric}"))?;
        Ok(data
            .get("metrics")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("malformed paired metrics for {metric}"))?)
    };
    let values = [
        metric_values("Ir")?,
        metric_values("I1mr")?,
        metric_values("ILmr")?,
    ];
    if values.iter().all(|counts| {
        !counts.contains_key("Both") && counts.get("Left").and_then(integer_count).is_some()
    }) {
        return Ok(None);
    }
    let mut pairs = [(0, 0); 3];
    for ((metric, values), pair) in METRICS.iter().zip(values).zip(&mut pairs) {
        let counts = values
            .get("Both")
            .filter(|value| !value.is_null())
            .ok_or_else(|| format!("missing paired counts for {metric}"))?;
        let invalid = || format!("invalid paired integer counts for {metric}");
        let counts = counts
            .as_array()
            .filter(|counts| counts.len() == 2)
            .ok_or_else(invalid)?;
        *pair = (
            integer_count(&counts[0]).ok_or_else(invalid)?,
            integer_count(&counts[1]).ok_or_else(invalid)?,
        );
    }
    Ok(Some(pairs))
}

fn result_from_pairs(id: &BenchmarkId, pairs: Pairs) -> ToolResult<WorkloadResult> {
    let candidate = pairs.map(|pair| pair.0);
    let baseline = pairs.map(|pair| pair.1);
    let mut failed_metrics = Vec::new();
    for ((metric, baseline), candidate) in METRICS.iter().zip(baseline).zip(candidate) {
        if !within_limit(metric, baseline, candidate)? {
            failed_metrics.push(*metric);
        }
    }
    let passed = failed_metrics.is_empty();
    Ok(WorkloadResult {
        name: display_id(id),
        candidate,
        baseline,
        passed,
        id: id.clone(),
        summary_path: PathBuf::new(),
        failed_metrics,
    })
}

fn compare_tree(
    root: &Path,
    baseline_name: &str,
    baseline_ids: &BTreeSet<BenchmarkId>,
) -> ToolResult<Comparison> {
    let mut selected = BTreeMap::new();
    for path in summary_paths(root)? {
        let summary = load_json(&path)?;
        if !has_selected_baseline(&summary, baseline_name)? {
            continue;
        }
        let id = canonical_id(&summary)?;
        match selected.entry(id) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                return Err(format!(
                    "duplicate candidate benchmark ID {} in {}",
                    display_id(entry.key()),
                    path.display()
                )
                .into());
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((summary, path));
            }
        }
    }
    let pairs: BTreeMap<BenchmarkId, (Option<Pairs>, PathBuf)> = selected
        .into_iter()
        .map(|(id, (summary, path))| Ok((id, (paired_counts(callgrind_metrics(&summary)?)?, path))))
        .collect::<ToolResult<_>>()?;
    let missing: Vec<String> = baseline_ids
        .iter()
        .filter(|id| !matches!(pairs.get(*id), Some((Some(_), _))))
        .map(|id| format!("missing baseline benchmark ID {}", display_id(id)))
        .collect();
    if !missing.is_empty() {
        return Err(missing.join("; ").into());
    }
    let new_ids: Vec<BenchmarkId> = pairs
        .keys()
        .filter(|id| !baseline_ids.contains(*id))
        .cloned()
        .collect();
    let paired_new: Vec<String> = new_ids
        .iter()
        .filter(|id| matches!(pairs.get(*id), Some((Some(_), _))))
        .map(|id| {
            format!(
                "candidate benchmark is paired but absent from baseline manifest: {}",
                display_id(id)
            )
        })
        .collect();
    if !paired_new.is_empty() {
        return Err(paired_new.join("; ").into());
    }
    let results = baseline_ids
        .iter()
        .map(|id| {
            let (pair, path) = pairs.get(id).ok_or("missing baseline benchmark counts")?;
            let mut result =
                result_from_pairs(id, pair.ok_or("missing baseline benchmark counts")?)?;
            result.summary_path.clone_from(path);
            Ok(result)
        })
        .collect::<ToolResult<_>>()?;
    Ok(Comparison {
        results,
        new_ids,
        inputs: pairs,
    })
}

fn data_error(context: &str, error: &dyn fmt::Display) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(ExitError {
        status: 2,
        message: format!("{context}: {error}"),
    })
}

const PROVENANCE_FIELDS: [&str; 8] = [
    "target",
    "base_sha",
    "candidate_sha",
    "repository",
    "request_id",
    "workflow_sha",
    "run_id",
    "run_attempt",
];

fn input_identity(root: &Path, path: &Path) -> ToolResult<Value> {
    let path = path.canonicalize()?;
    let name = path
        .strip_prefix(root)?
        .to_str()
        .ok_or("Non-Unicode policy input path")?
        .replace('\\', "/");
    Ok(
        json!({"path": name, "sha256": crate::common::sha256_file(&path)?,
        "size": fs::metadata(&path)?.len()}),
    )
}

fn write_verdict(
    args: &Args,
    baseline_manifest: &Path,
    baseline_name: &str,
    comparison: &Comparison,
    output: &Path,
) -> ToolResult<()> {
    let manifest_path = args
        .artifact_manifest
        .as_ref()
        .ok_or("Missing artifact manifest")?;
    let manifest = load_json(manifest_path)?;
    if manifest["schema_version"] != 1 || manifest["mode"] != "callgrind" {
        return Err("Policy verdict requires a prepared Callgrind artifact manifest".into());
    }
    let root = manifest_path
        .parent()
        .ok_or("Missing manifest directory")?
        .canonicalize()?;
    // Keep inventory paths relative to the same bundle as the prepared manifest.
    if output
        .parent()
        .ok_or("Missing verdict directory")?
        .canonicalize()?
        != root
    {
        return Err("Policy verdict must be written beside its artifact manifest".into());
    }
    let mut provenance = serde_json::Map::new();
    for field in PROVENANCE_FIELDS {
        if manifest[field].as_str().is_none_or(str::is_empty) {
            return Err(format!("Missing policy source provenance: {field}").into());
        }
        provenance.insert(field.to_owned(), manifest[field].clone());
    }
    let baseline = input_identity(&root, baseline_manifest)?;
    let summaries: Vec<Value> = comparison
        .inputs
        .values()
        .map(|(_, path)| input_identity(&root, path))
        .collect::<ToolResult<_>>()?;
    let workloads: Vec<Value> = comparison.results.iter().map(|result| {
        let metrics: serde_json::Map<String, Value> = METRICS.iter().enumerate().map(|(index, metric)| {
            ((*metric).to_owned(), json!({"baseline": result.baseline[index],
                "candidate": result.candidate[index], "passed": !result.failed_metrics.contains(metric)}))
        }).collect();
        Ok(json!({"function_name": result.id.0, "id": result.id.1, "name": result.name,
            "summary_path": result.summary_path.canonicalize()?.strip_prefix(&root)?
                .to_str().ok_or("Non-Unicode policy input path")?.replace('\\', "/"),
            "metrics": metrics, "passed": result.passed, "failed_metrics": result.failed_metrics}))
    }).collect::<ToolResult<_>>()?;
    let passed = comparison.results.iter().all(|result| result.passed);
    let new_workloads: Vec<Value> = comparison
        .new_ids
        .iter()
        .map(|id| json!({"function_name": id.0, "id": id.1}))
        .collect();
    crate::common::write_json(
        output,
        &json!({
            "format_version": 1, "kind": "callgrind-policy",
            "status": if passed {"complete"} else {"regression"}, "passed": passed,
            "baseline_name": baseline_name, "provenance": provenance, "gate_identity": manifest["gate_identity"],
            "inputs": {"baseline_manifest": baseline, "summaries": summaries},
            "workload_count": comparison.results.len(),
            "passed_count": comparison.results.iter().filter(|result| result.passed).count(),
            "workloads": workloads, "new_workloads": new_workloads
        }),
    )
}

fn validate_workload_metrics(workload: &Value, pairs: Pairs) -> ToolResult<Vec<String>> {
    let metrics = workload["metrics"]
        .as_object()
        .ok_or("Missing verdict numeric metrics")?;
    if metrics.len() != METRICS.len() {
        return Err("Unexpected verdict metrics".into());
    }
    let mut failed = Vec::new();
    let mut evidence = Vec::new();
    for (index, metric) in METRICS.iter().enumerate() {
        let metric_data = &workload["metrics"][*metric];
        let (candidate, baseline) = pairs[index];
        if metric_data["baseline"].as_u64() != Some(baseline)
            || metric_data["candidate"].as_u64() != Some(candidate)
        {
            return Err("Verdict numeric evidence differs from retained raw summary".into());
        }
        let passed = metric_data["passed"]
            .as_bool()
            .ok_or("Missing verdict metric outcome")?;
        if passed != within_limit(metric, baseline, candidate)? {
            return Err("Verdict metric outcome differs from authoritative policy".into());
        }
        if !passed {
            failed.push(*metric);
            evidence.push(format!("{metric} {baseline}→{candidate}"));
        }
    }
    if workload["passed"].as_bool() != Some(failed.is_empty())
        || workload["failed_metrics"] != json!(failed)
    {
        return Err("Inconsistent verdict workload outcome".into());
    }
    Ok(evidence)
}

fn verdict_baseline_ids(
    verdict: &Value,
    inputs: &BTreeMap<String, Value>,
) -> ToolResult<BTreeSet<BenchmarkId>> {
    let baseline_path = verdict["inputs"]["baseline_manifest"]["path"]
        .as_str()
        .ok_or("Missing verdict baseline manifest identity")?;
    let baseline = inputs
        .get(baseline_path)
        .ok_or("Missing verified baseline manifest")?;
    if baseline["format_version"] != 1 {
        return Err("Unsupported verdict baseline manifest".into());
    }
    let mut baseline_ids = BTreeSet::new();
    for entry in baseline["benchmarks"]
        .as_array()
        .ok_or("Missing baseline benchmark inventory")?
    {
        if !baseline_ids.insert(canonical_id(entry)?) {
            return Err("Duplicate verdict baseline benchmark ID".into());
        }
    }
    if baseline_ids.is_empty() {
        return Err("Empty verdict baseline benchmark inventory".into());
    }
    Ok(baseline_ids)
}

/// Validate retained producer evidence without running a second policy comparison.
/// The dashboard supplies only manifest-verified, confined input documents.
pub(crate) fn validate_verdict(
    verdict: &Value,
    manifest: &Value,
    inputs: &BTreeMap<String, Value>,
) -> ToolResult<Vec<String>> {
    if verdict["format_version"] != 1 || verdict["kind"] != "callgrind-policy" {
        return Err("Unsupported Callgrind policy verdict schema".into());
    }
    for field in PROVENANCE_FIELDS {
        if verdict["provenance"][field] != manifest[field]
            || manifest[field].as_str().is_none_or(str::is_empty)
        {
            return Err(format!("Callgrind policy {field} differs from source provenance").into());
        }
    }
    if verdict["gate_identity"] != manifest["gate_identity"] {
        return Err("Callgrind policy gate identity differs from source manifest".into());
    }
    let baseline_ids = verdict_baseline_ids(verdict, inputs)?;
    let baseline_name = verdict["baseline_name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .ok_or("Missing verdict baseline name")?;
    let mut summaries = BTreeMap::new();
    for identity in verdict["inputs"]["summaries"]
        .as_array()
        .ok_or("Missing verdict summary inventory")?
    {
        let path = identity["path"]
            .as_str()
            .ok_or("Missing verdict summary path")?;
        let summary = inputs.get(path).ok_or("Missing verified verdict summary")?;
        if !has_selected_baseline(summary, baseline_name)? {
            return Err("Verdict summary uses a different baseline".into());
        }
        if summaries
            .insert(
                canonical_id(summary)?,
                (path, paired_counts(callgrind_metrics(summary)?)?),
            )
            .is_some()
        {
            return Err("Duplicate verdict summary benchmark ID".into());
        }
    }
    let workloads = verdict["workloads"]
        .as_array()
        .ok_or("Missing verdict workloads")?;
    let mut seen = BTreeSet::new();
    let mut failures = Vec::new();
    for workload in workloads {
        let id = canonical_id(workload)?;
        if !baseline_ids.contains(&id) || !seen.insert(id.clone()) {
            return Err("Verdict workload is duplicate or absent from baseline manifest".into());
        }
        let (path, pairs) = summaries
            .get(&id)
            .ok_or("Missing verdict workload summary")?;
        let pairs = pairs.ok_or("Unpaired verdict workload summary")?;
        if workload["summary_path"].as_str() != Some(*path) || workload["name"] != display_id(&id) {
            return Err("Verdict workload identity differs from raw summary".into());
        }
        let evidence = validate_workload_metrics(workload, pairs)?;
        if !evidence.is_empty() {
            failures.push(format!(
                "Callgrind FAIL {}: {}",
                display_id(&id),
                evidence.join(", ")
            ));
        }
    }
    if seen != baseline_ids
        || verdict["workload_count"].as_u64() != Some(workloads.len() as u64)
        || verdict["passed_count"].as_u64() != Some((workloads.len() - failures.len()) as u64)
        || verdict["passed"].as_bool() != Some(failures.is_empty())
        || verdict["status"]
            != if failures.is_empty() {
                "complete"
            } else {
                "regression"
            }
    {
        return Err("Incomplete or inconsistent Callgrind policy verdict".into());
    }
    let new_workloads = verdict["new_workloads"]
        .as_array()
        .ok_or("Missing verdict new-workload inventory")?;
    let new_ids: BTreeSet<BenchmarkId> = new_workloads
        .iter()
        .map(canonical_id)
        .collect::<ToolResult<_>>()?;
    let expected_new: BTreeSet<BenchmarkId> = summaries
        .keys()
        .filter(|id| !baseline_ids.contains(*id))
        .cloned()
        .collect();
    if new_ids.len() != new_workloads.len()
        || new_ids != expected_new
        || summaries
            .iter()
            .any(|(id, (_, pairs))| !baseline_ids.contains(id) && pairs.is_some())
    {
        return Err("Inconsistent verdict new-workload inventory".into());
    }
    Ok(failures)
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    let args = Args::try_parse_from(args)?;
    let outcome = execute(&args);
    if let Err(error) = &outcome
        && error
            .downcast_ref::<ExitError>()
            .is_none_or(|error| error.status != 1)
        && let Some(path) = &args.verdict
    {
        crate::common::write_json(
            path,
            &json!({
                "format_version": 1, "kind": "callgrind-policy", "status": "failed",
                "passed": false, "error": error.to_string()
            }),
        )?;
    }
    outcome
}

fn execute(args: &Args) -> ToolResult<()> {
    if !args.summary_root.is_dir() {
        return Err(data_error(
            "summary root is not a directory",
            &args.summary_root.display(),
        ));
    }
    if let Some(path) = &args.write_baseline_manifest {
        let count = write_baseline_manifest(&args.summary_root, path)
            .map_err(|error| data_error("cannot write baseline manifest", &error))?;
        println!("Wrote baseline manifest with {count} unique benchmarks.");
        return Ok(());
    }
    let baseline_name = args
        .baseline_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(ExitError {
                status: 2,
                message: "--baseline-name is required with --baseline-manifest".to_owned(),
            })
        })?;
    let path = args.baseline_manifest.as_ref().ok_or_else(
        || -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(ExitError {
                status: 2,
                message: "--baseline-manifest is required".to_owned(),
            })
        },
    )?;
    let baseline_ids = load_baseline_manifest(path)
        .map_err(|error| data_error("cannot compare Iai summaries", &error))?;
    let comparison = compare_tree(&args.summary_root, baseline_name, &baseline_ids)
        .map_err(|error| data_error("cannot compare Iai summaries", &error))?;
    if let Some(output) = &args.verdict {
        write_verdict(args, path, baseline_name, &comparison, output)
            .map_err(|error| data_error("cannot retain policy verdict", &error))?;
    }
    let Comparison {
        results, new_ids, ..
    } = comparison;
    let passed = results.iter().filter(|result| result.passed).count();
    for result in results.iter().filter(|result| !result.passed) {
        let mut failed_metrics = Vec::new();
        for metric in &result.failed_metrics {
            let index = METRICS
                .iter()
                .position(|name| name == metric)
                .ok_or("unknown failed policy metric")?;
            failed_metrics.push(format!(
                "{metric} {}→{}",
                result.baseline[index], result.candidate[index]
            ));
        }
        println!("FAIL {}: {}", result.name, failed_metrics.join(", "));
    }
    println!("{passed}/{} baseline workloads passed.", results.len());
    println!("{} new candidate workloads:", new_ids.len());
    for id in new_ids {
        println!("NEW {}", display_id(&id));
    }
    if passed != results.len() {
        return Err(Box::new(ExitError {
            status: 1,
            message: String::new(),
        }));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        active_allowance, callgrind_metrics, canonical_id, paired_counts, result_from_pairs,
        within_limit,
    };
    use crate::ToolResult;
    use serde_json::json;

    #[test]
    fn summary_pairs_candidate_first_and_baseline_second() -> ToolResult<()> {
        let summary = json!({
            "function_name": "synthetic_workload",
            "baselines": [null, "selected"],
            "profiles": [{"summaries": {"parts": [{"metrics_summary": {"Callgrind": {
                "Ir": {"metrics": {"Both": [{"Int": 900}, {"Int": 1000}]}},
                "I1mr": {"metrics": {"Both": [{"Int": 10}, {"Int": 100}]}},
                "ILmr": {"metrics": {"Both": [{"Int": 10}, {"Int": 100}]}}
            }}}]}}]
        });
        let id = canonical_id(&summary)?;
        let pairs = paired_counts(callgrind_metrics(&summary)?)?.ok_or("missing fixture pair")?;
        let result = result_from_pairs(&id, pairs)?;
        assert_eq!(result.candidate, [900, 10, 10]);
        assert_eq!(result.baseline, [1000, 100, 100]);
        assert!(result.passed);
        assert_eq!(result.name, "synthetic_workload");
        Ok(())
    }

    #[test]
    fn cache_event_boundaries() -> ToolResult<()> {
        for metric in ["I1mr", "ILmr"] {
            for (baseline, candidate, expected) in [
                (2, 3, true),
                (2, 6, false),
                (62, 65, true),
                (62, 66, false),
                (1000, 1029, true),
                (1000, 1030, true),
                (1000, 1031, false),
                (0, 3, true),
                (0, 4, false),
                (101, 105, true),
                (101, 106, false),
            ] {
                assert_eq!(
                    within_limit(metric, baseline, candidate)?,
                    expected,
                    "{metric} {baseline}→{candidate}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn instructions_keep_two_percent_relative_limit() -> ToolResult<()> {
        assert!(within_limit("Ir", 1000, 1020)?);
        assert!(!within_limit("Ir", 1000, 1021)?);
        assert!(within_limit("Ir", 0, 0)?);
        assert!(!within_limit("Ir", 0, 1)?);
        assert_eq!(active_allowance("Ir", 99)?, 1);
        Ok(())
    }

    #[test]
    fn arithmetic_does_not_overflow() -> ToolResult<()> {
        for metric in ["Ir", "I1mr", "ILmr"] {
            assert!(within_limit(metric, u64::MAX, u64::MAX)?);
        }
        assert!(within_limit("Ir", u64::MAX / 2, u64::MAX / 2)?);
        assert!(within_limit("Dr", 100, 100).is_err());
        Ok(())
    }
}
