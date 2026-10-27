use super::{
    BENCHMARKS, Identity, METRICS, PROBE, ROLES, ReportArgs, VARIANTS, baseline_name,
    callgrind::{identity, parse_measurement, signals, strict_json},
    require, shard,
};
use crate::{ToolResult, common::write_json, policy};
use serde_json::{Map, Value, json};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
};

const ENVIRONMENT_FIELDS: [&str; 7] = [
    "rustc",
    "cargo",
    "iai_runner",
    "valgrind",
    "os",
    "arch",
    "cache_args",
];

fn nonempty(value: &Value) -> bool {
    value.as_str().is_some_and(|text| !text.trim().is_empty())
}
fn digest_str(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn digest(value: &Value) -> bool {
    value.as_str().is_some_and(digest_str)
}

fn validate_metadata(data: &Value) -> ToolResult<()> {
    require(data.is_object(), "dataset must be an object")?;
    require(
        matches!(data["format_version"].as_u64(), Some(1 | 2)),
        "format_version must be integer 1 or 2",
    )?;
    for field in ["job_id", "source_revision"] {
        require(
            nonempty(&data[field]),
            &format!("{field} must be a nonempty string"),
        )?;
    }
    require(
        data["role"]
            .as_str()
            .is_some_and(|role| ROLES.contains(&role)),
        "role must be discovery or validation",
    )?;
    require(
        digest(&data["fixture_sha256"]),
        "fixture_sha256 must be a lowercase SHA256",
    )?;
    if data["format_version"] == 2 {
        let harnesses = benchmark_map(&data["harness_sha256"])?;
        require(
            harnesses.values().all(digest),
            "harness hashes must be lowercase SHA256",
        )?;
    } else {
        require(
            digest(&data["harness_sha256"]),
            "harness_sha256 must be a lowercase SHA256",
        )?;
    }
    let environment = data["environment"]
        .as_object()
        .ok_or("environment must be an object")?;
    for field in ENVIRONMENT_FIELDS {
        if field == "cache_args" {
            require(
                environment
                    .get(field)
                    .and_then(Value::as_array)
                    .is_some_and(|args| !args.is_empty() && args.iter().all(nonempty)),
                "environment.cache_args must be a nonempty string list",
            )?;
        } else {
            require(
                environment.get(field).is_some_and(nonempty),
                &format!("environment.{field} must be a nonempty string"),
            )?;
        }
    }
    require(
        environment
            .keys()
            .all(|key| ENVIRONMENT_FIELDS.contains(&key.as_str()) || key == "cache_geometry"),
        "environment must contain only comparable toolchain/cache fields; put host metadata in host",
    )?;
    require(
        data.get("host").is_none_or(Value::is_object),
        "host must be an object when present",
    )?;
    validate_variants(data)
}

fn validate_variants(data: &Value) -> ToolResult<()> {
    let variants = data["variants"]
        .as_object()
        .ok_or("variants must be an object")?;
    require(
        variants.len() == 5
            && VARIANTS
                .iter()
                .all(|variant| variants.contains_key(*variant)),
        "variants must contain exactly a, b, layout, extra_work, extra_alloc",
    )?;
    let mut ownership = None;
    for (name, variant) in variants {
        require(variant.is_object(), "variant must be an object")?;
        require(
            digest(&variant["source_sha256"]),
            "variant source hash must be a lowercase SHA256",
        )?;
        if data["format_version"] == 2 {
            let benchmarks = benchmark_map(&variant["benchmarks"])?;
            let mut current = BTreeMap::new();
            let mut union = BTreeSet::new();
            let mut executables = BTreeSet::new();
            for benchmark in BENCHMARKS {
                let manifest = &benchmarks[benchmark];
                validate_binary(manifest)?;
                require(
                    executables.insert(manifest["executable"].as_str()),
                    "benchmark executables must be distinct",
                )?;
                let identities = manifest_identities(&manifest["workloads"])?;
                require(
                    identities.is_disjoint(&union),
                    "benchmark workload ownership must be disjoint",
                )?;
                union.extend(identities.iter().cloned());
                if benchmark == "analysis" {
                    validate_probe(manifest, name)?;
                    require(
                        manifest["layout_probe_collected"] == false,
                        "analysis layout probe must have no collected execution",
                    )?;
                    require(
                        signals().is_subset(&identities),
                        "missing small/medium/large inlay_hints_warm controls",
                    )?;
                } else {
                    require(
                        manifest.get("layout_probe").is_none()
                            && manifest.get("layout_probe_collected").is_none(),
                        "reference benchmark must not declare an analysis layout probe",
                    )?;
                    require(
                        ["small", "medium", "large"].iter().all(|size| {
                            identities.contains(&(
                                "reference_location_order_warm".to_owned(),
                                Some((*size).to_owned()),
                            ))
                        }),
                        "missing small/medium/large reference_location_order_warm controls",
                    )?;
                }
                current.insert(benchmark, identities);
            }
            if let Some(previous) = &ownership {
                require(
                    previous == &current,
                    "benchmark workload identities or ownership differ between variants",
                )?;
            }
            ownership = Some(current);
        } else {
            validate_binary(variant)?;
            validate_probe(variant, name)?;
        }
    }
    require(
        variants["a"]["source_sha256"] == variants["b"]["source_sha256"],
        "A/A source hashes differ",
    )
}

fn benchmark_map(value: &Value) -> ToolResult<&Map<String, Value>> {
    let benchmarks = value.as_object().ok_or("benchmarks must be an object")?;
    require(
        benchmarks.len() == BENCHMARKS.len()
            && BENCHMARKS.iter().all(|name| benchmarks.contains_key(*name)),
        "benchmarks must contain exactly analysis and reference_locations",
    )?;
    Ok(benchmarks)
}

fn manifest_identities(value: &Value) -> ToolResult<BTreeSet<Identity>> {
    let rows = value
        .as_array()
        .filter(|rows| !rows.is_empty())
        .ok_or("manifest workloads must be a nonempty list")?;
    let mut identities = BTreeSet::new();
    for row in rows {
        require(
            row.as_object().is_some_and(|row| row.len() == 2),
            "manifest identity must contain only function_name and id",
        )?;
        require(
            identities.insert(identity(row)?),
            "duplicate manifest workload identity",
        )?;
    }
    Ok(identities)
}

fn validate_binary(manifest: &Value) -> ToolResult<()> {
    require(manifest.is_object(), "benchmark manifest must be an object")?;
    require(
        digest(&manifest["binary_sha256"]),
        "variant binary hash must be a lowercase SHA256",
    )?;
    require(
        nonempty(&manifest["executable"]),
        "variant executable must be nonempty",
    )
}

fn validate_probe(manifest: &Value, variant: &str) -> ToolResult<()> {
    let probe = &manifest["layout_probe"];
    require(probe.is_object(), "variant layout_probe must be an object")?;
    require(
        probe["symbol"].as_str().is_some_and(|symbol| {
            !symbol.trim().is_empty() && symbol.rsplit("::").next() == Some(PROBE)
        }),
        "variant must retain calibration_layout_probe",
    )?;
    require(
        probe["address"].as_str().is_some_and(|address| {
            !address.is_empty()
                && address.chars().all(|char| char.is_ascii_hexdigit())
                && address.chars().any(|char| char != '0')
        }),
        "variant probe address must be nonzero hexadecimal",
    )?;
    require(
        probe["size"]
            .as_u64()
            .is_some_and(|size| size >= if variant == "layout" { 1024 } else { 1 }),
        "variant probe size is invalid",
    )?;
    require(
        manifest
            .get("layout_probe_collected")
            .is_none_or(|value| value == false),
        "variant layout probe must have no collected execution",
    )
}

fn evidence_path(root: &Path, sample: &Value, field: &str) -> ToolResult<PathBuf> {
    let text = sample[field]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .ok_or("sample evidence must be a relative path")?;
    let path = Path::new(text);
    require(
        !path.is_absolute()
            && !path
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_))),
        "sample evidence must not escape the dataset directory",
    )?;
    let resolved = root.join(path).canonicalize().map_err(|error| {
        format!("sample evidence file is missing or escapes the dataset directory: {error}")
    })?;
    require(
        resolved.starts_with(root.canonicalize()?) && resolved.is_file(),
        "sample evidence file is missing or escapes the dataset directory",
    )?;
    Ok(resolved)
}

fn workload_identities(workloads: &Value) -> ToolResult<BTreeSet<Identity>> {
    let workloads = workloads
        .as_array()
        .filter(|rows| !rows.is_empty())
        .ok_or("workloads must be a nonempty list")?;
    let mut current = BTreeSet::new();
    for workload in workloads {
        require(workload.is_object(), "workload must be an object")?;
        require(
            current.insert(identity(workload)?),
            "duplicate workload identity",
        )?;
        let counts = workload["counts"]
            .as_object()
            .ok_or("workload counts must contain exactly Ir, I1mr, ILmr")?;
        require(
            counts.len() == 3 && METRICS.iter().all(|metric| counts.contains_key(*metric)),
            "workload counts must contain exactly Ir, I1mr, ILmr",
        )?;
        require(
            counts.values().all(|value| value.as_u64().is_some()),
            "metric counts must be nonnegative integers (not bool)",
        )?;
    }
    Ok(current)
}

fn collection_runner(stdout: &Path, root: &Path, evidence: &Value) -> ToolResult<String> {
    let collection = evidence["execution"]["collection"]
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .ok_or("benchmark execution must retain its runner collection receipt")?;
    // Producer layout: collection.json sits in the pair directory that contains
    // every benchmark child directory and its stdout/stderr evidence.
    let pair = stdout
        .parent()
        .ok_or("benchmark evidence has no benchmark directory")?
        .parent()
        .ok_or("benchmark evidence is not inside a pair directory")?;
    let path = pair
        .join(collection)
        .canonicalize()
        .map_err(|error| format!("benchmark collection receipt is missing: {error}"))?;
    require(
        path.starts_with(root.canonicalize()?) && path.is_file(),
        "benchmark collection receipt is missing or escapes the dataset directory",
    )?;
    let receipt = strict_json(&fs::read_to_string(&path)?)?;
    let runner = receipt["original_runner_sha256"]
        .as_str()
        .filter(|value| digest_str(value))
        .ok_or("benchmark collection receipt has no pinned runner digest")?;
    Ok(runner.to_owned())
}

#[derive(Clone, Copy)]
struct EvidenceScope<'a> {
    data: &'a Value,
    root: &'a Path,
    expected: &'a BTreeMap<&'static str, BTreeSet<Identity>>,
    require_profiles: bool,
}

fn benchmark_measurements(
    scope: EvidenceScope<'_>,
    sample: &Value,
    baseline: &str,
    variant: &str,
    evidence_files: &mut BTreeSet<PathBuf>,
) -> ToolResult<(Vec<Value>, BTreeSet<String>)> {
    let EvidenceScope {
        data,
        root,
        expected,
        require_profiles,
    } = scope;
    let benchmarks = sample["benchmarks"]
        .as_object()
        .ok_or("benchmark evidence must be an object")?;
    let measured_benchmarks: BTreeSet<_> = BENCHMARKS
        .into_iter()
        .filter(|benchmark| !expected[*benchmark].is_empty())
        .collect();
    require(
        benchmarks.len() == measured_benchmarks.len()
            && benchmarks
                .keys()
                .all(|key| measured_benchmarks.contains(key.as_str())),
        "benchmark evidence must cover exactly the shard's nonempty benchmarks",
    )?;
    let mut measured = Vec::with_capacity(expected.values().map(BTreeSet::len).sum());
    let mut runners = BTreeSet::new();
    for benchmark in BENCHMARKS {
        if expected[benchmark].is_empty() {
            continue;
        }
        let evidence = &benchmarks[benchmark];
        let stdout = evidence_path(root, evidence, "raw_stdout")?;
        let stderr = evidence_path(root, evidence, "raw_stderr")?;
        require(
            evidence_files.insert(stdout.clone()) && evidence_files.insert(stderr),
            "benchmark raw evidence files must be distinct for every execution",
        )?;
        require(
            evidence["verified_callgrind_profiles"]
                .as_u64()
                .is_some_and(|count| count >= expected[benchmark].len() as u64),
            "benchmark verified Callgrind profile count is missing or incomplete",
        )?;
        if require_profiles {
            runners.insert(collection_runner(&stdout, root, evidence)?);
        }
        let executable = Path::new(
            data["variants"][variant]["benchmarks"][benchmark]["executable"]
                .as_str()
                .ok_or("executable missing")?,
        );
        let (rows, _) = parse_measurement(
            &fs::read_to_string(stdout)?,
            baseline,
            executable,
            &expected[benchmark],
        )?;
        measured.extend(rows);
    }
    Ok((measured, runners))
}

impl EvidenceScope<'_> {
    fn measured(
        &self,
        sample: &Value,
        baseline: &str,
        variant: &str,
        evidence_files: &mut BTreeSet<PathBuf>,
    ) -> ToolResult<(Vec<Value>, BTreeSet<String>)> {
        if self.data["format_version"] == 2 {
            return benchmark_measurements(*self, sample, baseline, variant, evidence_files);
        }
        let expected: BTreeSet<_> = self.expected.values().flatten().cloned().collect();
        let stdout = evidence_path(self.root, sample, "raw_stdout")?;
        evidence_path(self.root, sample, "raw_stderr")?;
        let executable = Path::new(
            self.data["variants"][variant]["executable"]
                .as_str()
                .ok_or("executable missing")?,
        );
        let (rows, _) = parse_measurement(
            &fs::read_to_string(stdout)?,
            baseline,
            executable,
            &expected,
        )?;
        Ok((rows, BTreeSet::new()))
    }
}

fn validate_sample_counts(sample: &Value, measured: &[Value]) -> ToolResult<()> {
    let measured = measured
        .iter()
        .map(|row| Ok((identity(row)?, &row["counts"])))
        .collect::<ToolResult<BTreeMap<_, _>>>()?;
    let declared = sample["workloads"]
        .as_array()
        .ok_or("workloads missing")?
        .iter()
        .map(|row| Ok((identity(row)?, &row["counts"])))
        .collect::<ToolResult<BTreeMap<_, _>>>()?;
    require(
        measured == declared,
        "dataset counts contradict retained raw stdout",
    )
}

fn selected_identities(value: &Value) -> ToolResult<BTreeSet<Identity>> {
    let units = value.as_array().ok_or("selected units must be a list")?;
    let mut names = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for unit in units {
        require(
            unit.as_object().is_some_and(|unit| {
                unit.len() == 2 && unit.contains_key("function_name") && unit.contains_key("ids")
            }),
            "selected unit must contain only function_name and ids",
        )?;
        let name = unit["function_name"]
            .as_str()
            .filter(|name| !name.trim().is_empty())
            .ok_or("selected unit function_name must be nonempty")?;
        require(
            names.insert(name.to_owned()),
            "duplicate selected function unit",
        )?;
        let cases = unit["ids"]
            .as_array()
            .filter(|cases| !cases.is_empty())
            .ok_or("selected function unit must retain at least one case")?;
        for id in cases {
            let identifier = match id {
                Value::Null => None,
                Value::String(identifier) if !identifier.trim().is_empty() => {
                    Some(identifier.clone())
                }
                _ => return Err("selected unit id must be null or a nonempty string".into()),
            };
            require(
                identities.insert((name.to_owned(), identifier)),
                "duplicate selected unit identity",
            )?;
        }
    }
    Ok(identities)
}

/// Validate one physical dataset and, when present, its deterministic shard
/// declaration against the frozen manifest retained in every variant.
fn shard_selection(
    data: &Value,
    full: &BTreeMap<&'static str, BTreeSet<Identity>>,
) -> ToolResult<BTreeMap<&'static str, BTreeSet<Identity>>> {
    let shard = data["shard"]
        .as_object()
        .ok_or("shard provenance must be an object")?;
    require(
        data["format_version"] == 2,
        "sharded datasets require format_version 2",
    )?;
    require(
        shard.len() == 11,
        "shard provenance must contain exactly the declared fields",
    )?;
    require(
        shard["method"] == shard::METHOD,
        "shard provenance must declare the deterministic partition method",
    )?;
    let count = shard["count"]
        .as_u64()
        .ok_or("shard count must be an integer")?;
    let index = shard["index"]
        .as_u64()
        .ok_or("shard index must be an integer")?;
    require(
        (1..=shard::MAX_COUNT).contains(&count),
        "shard count must be between 1 and 4",
    )?;
    require(index < count, "shard index must be less than shard count")?;
    for field in [
        "logical_job_id",
        "physical_job_id",
        "matrix_job",
        "run_id",
        "run_attempt",
        "physical_runner",
    ] {
        require(
            shard.get(field).is_some_and(nonempty),
            &format!("shard {field} must be a nonempty string"),
        )?;
    }
    let logical = shard["logical_job_id"]
        .as_str()
        .ok_or("logical job missing")?;
    require(
        logical == data["job_id"].as_str().ok_or("job_id missing")?,
        "shard logical job must equal job_id",
    )?;
    let physical = if count == 1 {
        logical.to_owned()
    } else {
        format!("{logical}-shard-{index}")
    };
    require(
        shard["physical_job_id"] == physical,
        "physical job identity must match logical job and shard index",
    )?;
    require(
        shard["matrix_job"] == physical,
        "matrix job must identify the physical shard job",
    )?;
    require(
        data["host"]["runner_identity"].as_str() == shard["physical_runner"].as_str(),
        "physical runner identity must match retained host metadata",
    )?;
    require(
        shard["full_manifest"] == shard::manifest(full)?,
        "declared full manifest is stale or incomplete",
    )?;
    let selected = shard::selection(full, count, index)?;
    require(
        shard["selected_units"] == shard::units(&selected),
        "selected units do not match the deterministic shard partition",
    )?;
    Ok(selected)
}

pub(super) fn validate_job(data: &Value, root: &Path) -> ToolResult<Option<String>> {
    validate_metadata(data)?;
    let samples = data["samples"]
        .as_array()
        .filter(|samples| !samples.is_empty())
        .ok_or("samples must be a nonempty list")?;
    let full = benchmark_identities(data)?;
    let declared = data.get("shard").is_some();
    let expected = if declared {
        shard_selection(data, &full)?
    } else {
        full.clone()
    };
    let expected_identities: BTreeSet<_> = expected.values().flatten().cloned().collect();
    require(
        !expected_identities.is_empty(),
        "declared workload scope must be nonempty",
    )?;
    let mut seen = BTreeSet::new();
    let mut orders: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    let mut evidence_files = BTreeSet::new();
    let mut profiles = BTreeSet::new();
    for sample in samples {
        require(sample.is_object(), "sample must be an object")?;
        let pair = sample["pair"]
            .as_u64()
            .filter(|pair| *pair > 0)
            .ok_or("pair must be a positive integer")?;
        let variant = sample["variant"]
            .as_str()
            .filter(|variant| VARIANTS.contains(variant))
            .ok_or("sample variant is unsupported")?;
        let order = sample["order"]
            .as_u64()
            .ok_or("order must be a nonnegative integer")?;
        require(
            seen.insert((pair, variant)),
            "duplicate (pair, variant) sample",
        )?;
        require(
            orders.entry(pair).or_default().insert(order),
            "duplicate execution order within pair",
        )?;
        let baseline = baseline_name(
            data["job_id"].as_str().ok_or("job_id missing")?,
            pair,
            variant,
        );
        let scope = EvidenceScope {
            data,
            root,
            expected: &expected,
            require_profiles: declared,
        };
        let (measured, runners) =
            scope.measured(sample, &baseline, variant, &mut evidence_files)?;
        if declared {
            require(
                runners.len() == 1,
                "benchmark evidence must retain one pinned runner identity",
            )?;
            profiles.extend(runners);
        }
        validate_sample_counts(sample, &measured)?;
        require(
            workload_identities(&sample["workloads"])? == expected_identities,
            "sample workloads do not match the declared scope",
        )?;
    }
    if declared {
        require(
            profiles.len() == 1,
            "all benchmark evidence must use the same pinned runner",
        )?;
    } else {
        require(
            signals().is_subset(&expected_identities),
            "missing small/medium/large inlay_hints_warm controls",
        )?;
    }
    validate_pairs(&seen)?;
    Ok(profiles.into_iter().next())
}

fn validate_pairs(seen: &BTreeSet<(u64, &str)>) -> ToolResult<()> {
    let pairs: BTreeSet<_> = seen.iter().map(|(pair, _)| *pair).collect();
    require(
        pairs.first() == Some(&1) && pairs.last().copied() == Some(u64::try_from(pairs.len())?),
        "pair numbers must be contiguous from 1",
    )?;
    require(
        pairs.iter().all(|pair| {
            VARIANTS
                .iter()
                .all(|variant| seen.contains(&(*pair, *variant)))
        }),
        "every pair must contain all five variants",
    )
}

fn benchmark_identities(data: &Value) -> ToolResult<BTreeMap<&'static str, BTreeSet<Identity>>> {
    if data["format_version"] == 1 {
        return Ok(BTreeMap::from([(
            "analysis",
            workload_identities(&data["samples"][0]["workloads"])?,
        )]));
    }
    BENCHMARKS
        .into_iter()
        .map(|benchmark| {
            Ok((
                benchmark,
                manifest_identities(&data["variants"]["a"]["benchmarks"][benchmark]["workloads"])?,
            ))
        })
        .collect()
}

pub(super) fn benchmark_scope(data: &Value) -> ToolResult<Value> {
    let identities = benchmark_identities(data)?;
    let benchmarks: Map<_, _> = identities
        .iter()
        .map(|(name, identities)| ((*name).to_owned(), json!(identities.len())))
        .collect();
    let workloads = workload_identities(&data["samples"][0]["workloads"])?.len();
    Ok(
        json!({"evidence_format_version": data["format_version"], "benchmarks": benchmarks, "workloads": workloads}),
    )
}

fn dataset_paths(directory: &Path, root: &Path, paths: &mut Vec<PathBuf>) -> ToolResult<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == "data.json" {
            require(
                path.canonicalize()?.starts_with(root),
                "data.json must not escape the input root",
            )?;
            paths.push(path);
        } else if entry.file_type()?.is_dir() {
            dataset_paths(&path, root, paths)?;
        }
    }
    Ok(())
}

struct PhysicalDataset {
    path: PathBuf,
    data: Value,
}

fn relative_dataset(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn group_common(
    ordered: &[&PhysicalDataset],
    first: &Value,
) -> ToolResult<BTreeMap<&'static str, BTreeSet<Identity>>> {
    let role = first["role"].as_str().ok_or("role missing")?;
    for member in ordered {
        require(
            member.data["role"].as_str() == Some(role),
            "physical shards disagree on the logical role",
        )?;
        for field in [
            "format_version",
            "source_revision",
            "fixture_sha256",
            "harness_sha256",
            "environment",
        ] {
            require(
                member.data[field] == first[field],
                &format!("incomparable {field} across physical shards"),
            )?;
        }
        for variant in VARIANTS {
            require(
                member.data["variants"][variant]["source_sha256"]
                    == first["variants"][variant]["source_sha256"],
                &format!("incomparable {variant} source_sha256 across physical shards"),
            )?;
        }
    }
    let full = benchmark_identities(first)?;
    for member in &ordered[1..] {
        require(
            benchmark_identities(&member.data)? == full,
            "physical shards declare different full manifests",
        )?;
    }
    Ok(full)
}

fn group_scope(
    ordered: &[&PhysicalDataset],
    root: &Path,
    full: &BTreeMap<&'static str, BTreeSet<Identity>>,
) -> ToolResult<Vec<Value>> {
    let mut union: BTreeMap<&'static str, BTreeSet<Identity>> = BENCHMARKS
        .into_iter()
        .map(|benchmark| (benchmark, BTreeSet::new()))
        .collect();
    let mut workload_origin: BTreeMap<Identity, u64> = BTreeMap::new();
    let mut physical_ids = BTreeSet::new();
    let mut runs = BTreeSet::new();
    let mut provenance = Vec::with_capacity(ordered.len());
    for member in ordered {
        let declaration = &member.data["shard"];
        let index = declaration["index"].as_u64().ok_or("shard index missing")?;
        let physical = declaration["physical_job_id"]
            .as_str()
            .ok_or("physical job missing")?;
        require(
            physical_ids.insert(physical.to_owned()),
            "physical shard jobs must be unique",
        )?;
        runs.insert((
            declaration["run_id"]
                .as_str()
                .ok_or("run id missing")?
                .to_owned(),
            declaration["run_attempt"]
                .as_str()
                .ok_or("run attempt missing")?
                .to_owned(),
        ));
        for benchmark in BENCHMARKS {
            for identity in selected_identities(&declaration["selected_units"][benchmark])? {
                require(
                    workload_origin.insert(identity.clone(), index).is_none(),
                    "physical shards must not overlap workloads",
                )?;
                require(
                    union[benchmark].insert(identity),
                    "physical shards must not duplicate selected units",
                )?;
            }
        }
        let mut record = declaration.clone();
        record["dataset"] = json!(relative_dataset(root, &member.path));
        record["host"] = member.data["host"].clone();
        record["runner_sha256"] = member.data["runner_sha256"].clone();
        provenance.push(record);
    }
    require(
        runs.len() == 1,
        "mixed workflow run or attempt across physical shards",
    )?;
    require(
        &union == full,
        "physical shards do not cover the complete frozen workload scope",
    )?;
    Ok(provenance)
}

struct MergedSample {
    order: u64,
    workloads: Vec<Value>,
    identities: BTreeSet<Identity>,
    origins: BTreeSet<u64>,
}

fn collect_merged(
    ordered: &[&PhysicalDataset],
    full: &BTreeMap<&'static str, BTreeSet<Identity>>,
) -> ToolResult<Vec<Value>> {
    let expected_identities: BTreeSet<_> = full.values().flatten().cloned().collect();
    let all_indices: BTreeSet<u64> = ordered
        .iter()
        .filter_map(|member| member.data["shard"]["index"].as_u64())
        .collect();
    let mut merged: BTreeMap<(u64, usize), MergedSample> = BTreeMap::new();
    for member in ordered {
        let index = member.data["shard"]["index"]
            .as_u64()
            .ok_or("shard index missing")?;
        for sample in member.data["samples"].as_array().ok_or("samples missing")? {
            let pair = sample["pair"].as_u64().ok_or("pair missing")?;
            let variant = sample["variant"].as_str().ok_or("variant missing")?;
            let position = VARIANTS
                .iter()
                .position(|candidate| *candidate == variant)
                .ok_or("variant unsupported")?;
            let order = sample["order"].as_u64().ok_or("order missing")?;
            let entry = merged
                .entry((pair, position))
                .or_insert_with(|| MergedSample {
                    order,
                    workloads: Vec::new(),
                    identities: BTreeSet::new(),
                    origins: BTreeSet::new(),
                });
            require(
                entry.order == order,
                "physical shards disagree on balanced execution order",
            )?;
            require(
                entry.origins.insert(index),
                "duplicate sample within a physical shard",
            )?;
            for row in sample["workloads"].as_array().ok_or("workloads missing")? {
                require(
                    entry.identities.insert(identity(row)?),
                    "duplicate measurement across physical shards",
                )?;
                entry.workloads.push(row.clone());
            }
        }
    }
    let mut samples = Vec::with_capacity(merged.len());
    for ((pair, position), sample) in merged {
        require(
            sample.origins == all_indices,
            "every physical shard must contribute to every pair and variant",
        )?;
        require(
            sample.identities == expected_identities,
            "merged sample does not cover the complete frozen workload scope",
        )?;
        samples.push(json!({
            "pair": pair,
            "variant": VARIANTS[position],
            "order": sample.order,
            "workloads": sample.workloads,
            "shards": sample.origins.into_iter().collect::<Vec<_>>(),
        }));
    }
    Ok(samples)
}

fn merge_group(members: &[PhysicalDataset], root: &Path) -> ToolResult<Value> {
    if members.len() == 1 {
        let data = &members[0].data;
        if data.get("shard").is_some() {
            require(
                data["shard"]["count"] == 1,
                "a single dataset cannot declare multiple physical shards",
            )?;
        }
        return Ok(data.clone());
    }
    require(
        members
            .iter()
            .all(|member| member.data.get("shard").is_some()),
        "duplicate job_id",
    )?;
    let mut ordered: Vec<&PhysicalDataset> = members.iter().collect();
    ordered.sort_by_key(|member| member.data["shard"]["index"].as_u64().unwrap_or(u64::MAX));
    let count = ordered[0].data["shard"]["count"]
        .as_u64()
        .ok_or("shard count missing")?;
    require(
        u64::try_from(ordered.len())? == count,
        "logical job is missing or duplicates physical shards",
    )?;
    let indices: BTreeSet<u64> = ordered
        .iter()
        .filter_map(|member| member.data["shard"]["index"].as_u64())
        .collect();
    require(
        indices == (0..count).collect::<BTreeSet<_>>(),
        "physical shard indices must be unique, complete, and in range",
    )?;
    let first = &ordered[0].data;
    let full = group_common(&ordered, first)?;
    let provenance = group_scope(&ordered, root, &full)?;
    let runner = first["runner_sha256"]
        .as_str()
        .ok_or("pinned runner identity missing")?;
    for member in &ordered {
        require(
            member.data["runner_sha256"].as_str() == Some(runner),
            "incomparable Callgrind runner identity across physical shards",
        )?;
    }
    let samples = collect_merged(&ordered, &full)?;
    require(
        samples.len() == first["samples"].as_array().map_or(0, Vec::len),
        "physical shards disagree on the number of balanced samples",
    )?;
    Ok(json!({
        "format_version": first["format_version"],
        "job_id": first["job_id"],
        "role": first["role"],
        "source_revision": first["source_revision"],
        "fixture_sha256": first["fixture_sha256"],
        "harness_sha256": first["harness_sha256"],
        "environment": first["environment"],
        "variants": first["variants"],
        "host": {"logical_job_id": first["job_id"], "shards": provenance.iter().map(|record| json!({"index": record["index"], "physical_job_id": record["physical_job_id"], "physical_runner": record["physical_runner"], "host": record["host"]})).collect::<Vec<_>>()},
        "shards": provenance,
        "runner_sha256": runner,
        "samples": samples,
    }))
}

pub(super) fn read_datasets(root: &Path) -> ToolResult<Vec<Value>> {
    require(root.is_dir(), "input root must be a directory")?;
    let mut paths = Vec::new();
    dataset_paths(root, &root.canonicalize()?, &mut paths)?;
    paths.sort();
    require(!paths.is_empty(), "no data.json datasets found")?;
    let mut groups: BTreeMap<String, Vec<PhysicalDataset>> = BTreeMap::new();
    for path in paths {
        let mut job = strict_json(&fs::read_to_string(&path)?)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let parent = path.parent().ok_or("dataset directory missing")?;
        let runner =
            validate_job(&job, parent).map_err(|error| format!("{}: {error}", path.display()))?;
        if let Some(runner) = runner {
            job["runner_sha256"] = json!(runner);
        }
        let job_id = job["job_id"].as_str().ok_or("job_id missing")?.to_owned();
        groups
            .entry(job_id)
            .or_default()
            .push(PhysicalDataset { path, data: job });
    }
    let mut jobs = Vec::with_capacity(groups.len());
    for members in groups.into_values() {
        jobs.push(merge_group(&members, root)?);
    }
    validate_comparability(&jobs)?;
    Ok(jobs)
}

pub(super) fn validate_comparability(jobs: &[Value]) -> ToolResult<()> {
    let first = jobs.first().ok_or("no calibration jobs")?;
    let ids: BTreeSet<_> = jobs
        .iter()
        .filter_map(|job| job["job_id"].as_str())
        .collect();
    require(ids.len() == jobs.len(), "duplicate job_id")?;
    let roles: BTreeSet<_> = jobs.iter().filter_map(|job| job["role"].as_str()).collect();
    require(
        roles == BTreeSet::from(ROLES),
        "both discovery and held-out validation jobs are required",
    )?;
    let first_identities = workload_identities(&first["samples"][0]["workloads"])?;
    let first_benchmarks = benchmark_identities(first)?;
    for job in &jobs[1..] {
        for field in [
            "format_version",
            "source_revision",
            "fixture_sha256",
            "harness_sha256",
            "environment",
        ] {
            require(
                job[field] == first[field],
                &format!("incomparable {field} across jobs"),
            )?;
        }
        for variant in VARIANTS {
            require(
                job["variants"][variant]["source_sha256"]
                    == first["variants"][variant]["source_sha256"],
                &format!("incomparable {variant} source_sha256 across jobs"),
            )?;
        }
        require(
            workload_identities(&job["samples"][0]["workloads"])? == first_identities,
            "incomparable workload identities across jobs",
        )?;
        require(
            benchmark_identities(job)? == first_benchmarks,
            "incomparable benchmark workload ownership across jobs",
        )?;
    }
    let runners: BTreeSet<Option<&str>> = jobs
        .iter()
        .map(|job| job.get("runner_sha256").and_then(Value::as_str))
        .collect();
    require(
        runners.len() == 1,
        "incomparable Callgrind runner identity across jobs",
    )?;
    let mut runs = BTreeSet::new();
    let mut sharded = 0_usize;
    for job in jobs {
        if let Some(shards) = job["shards"].as_array() {
            sharded += 1;
            for shard in shards {
                runs.insert((
                    shard["run_id"].as_str().ok_or("run id missing")?.to_owned(),
                    shard["run_attempt"]
                        .as_str()
                        .ok_or("run attempt missing")?
                        .to_owned(),
                ));
            }
        } else if let Some(shard) = job.get("shard") {
            sharded += 1;
            runs.insert((
                shard["run_id"].as_str().ok_or("run id missing")?.to_owned(),
                shard["run_attempt"]
                    .as_str()
                    .ok_or("run attempt missing")?
                    .to_owned(),
            ));
        }
    }
    require(
        sharded == 0 || sharded == jobs.len(),
        "mixed sharded and unsharded calibration datasets",
    )?;
    require(
        runs.len() <= 1,
        "mixed workflow run or attempt across physical shards",
    )
}

pub(super) fn observed_shards(job: &Value) -> u64 {
    job["shards"].as_array().map_or_else(
        || job["shard"]["count"].as_u64().unwrap_or(1),
        |shards| u64::try_from(shards.len()).unwrap_or(0),
    )
}

pub(super) fn check_expected_shards(jobs: &[Value], expected: Option<u64>) -> ToolResult<()> {
    if let Some(expected) = expected {
        for job in jobs {
            let observed = observed_shards(job);
            require(
                observed == expected,
                &format!(
                    "job {}: expected {expected} physical shards, found {observed}",
                    job["job_id"]
                ),
            )?;
        }
    }
    Ok(())
}

fn comparisons(jobs: &[Value]) -> ToolResult<Vec<Value>> {
    let mut jobs: Vec<_> = jobs.iter().collect();
    jobs.sort_by(|left, right| left["job_id"].as_str().cmp(&right["job_id"].as_str()));
    let mut rows = Vec::new();
    for job in jobs {
        let mut pairs: BTreeMap<u64, BTreeMap<&str, &Value>> = BTreeMap::new();
        for sample in job["samples"].as_array().ok_or("samples missing")? {
            pairs
                .entry(sample["pair"].as_u64().ok_or("pair missing")?)
                .or_default()
                .insert(sample["variant"].as_str().ok_or("variant missing")?, sample);
        }
        for (pair, samples) in pairs {
            let baseline = samples.get("a").ok_or("baseline sample missing")?["workloads"]
                .as_array()
                .ok_or("workloads missing")?
                .iter()
                .map(|row| Ok((identity(row)?, &row["counts"])))
                .collect::<ToolResult<BTreeMap<_, _>>>()?;
            for variant in &VARIANTS[1..] {
                for workload in samples.get(variant).ok_or("control sample missing")?["workloads"]
                    .as_array()
                    .ok_or("workloads missing")?
                {
                    let id = identity(workload)?;
                    for metric in METRICS {
                        let before = baseline.get(&id).ok_or("baseline workload missing")?[metric]
                            .as_u64()
                            .ok_or("baseline count missing")?;
                        let after = workload["counts"][metric]
                            .as_u64()
                            .ok_or("candidate count missing")?;
                        let delta = i128::from(after) - i128::from(before);
                        rows.push(json!({"job_id": job["job_id"], "role": job["role"], "pair": pair, "variant": variant, "function_name": id.0, "id": id.1, "metric": metric, "baseline": before, "candidate": after, "absolute_delta": delta, "relative_delta": relative_float(delta, before), "relative_delta_exact": if before == 0 { Value::Null } else { json!({"numerator": delta, "denominator": before}) }, "current_passed": policy::within_limit(metric, before, after)?}));
                    }
                }
            }
        }
    }
    Ok(rows)
}

fn delta(row: &Value) -> ToolResult<i128> {
    Ok(row["absolute_delta"].to_string().parse()?)
}
fn relative_float(numerator: i128, denominator: u64) -> Option<f64> {
    if denominator == 0 {
        return None;
    }
    Some(json!(numerator).as_f64()? / json!(denominator).as_f64()?)
}

type FloorKey = (Identity, String);

fn learn_allowances(rows: &mut [Value]) -> ToolResult<Vec<Value>> {
    let mut floors: BTreeMap<FloorKey, i128> = BTreeMap::new();
    for row in rows.iter() {
        if row["role"] == "discovery" && row["variant"] == "b" && row["metric"] != "Ir" {
            let floor = floors
                .entry((
                    identity(row)?,
                    row["metric"].as_str().ok_or("metric missing")?.to_owned(),
                ))
                .or_default();
            *floor = (*floor).max(delta(row)?);
        }
    }
    let identities = rows
        .iter()
        .map(identity)
        .collect::<ToolResult<BTreeSet<_>>>()?;
    let allowances = identities.into_iter().flat_map(|(name, id)| METRICS[1..].iter().map(move |metric| (name.clone(), id.clone(), *metric)))
        .map(|(name, id, metric)| json!({"function_name": name, "id": id, "metric": metric, "discovery_positive_delta_floor": floors.get(&((name.clone(), id.clone()), metric.to_owned())).copied().unwrap_or_default()})).collect();
    for row in rows {
        let metric = row["metric"].as_str().ok_or("metric missing")?;
        let active = i128::from(policy::active_allowance(
            metric,
            row["baseline"].as_u64().ok_or("baseline missing")?,
        )?);
        let floor = if metric == "Ir" {
            0
        } else {
            floors
                .get(&(identity(row)?, metric.to_owned()))
                .copied()
                .unwrap_or_default()
        };
        let proposed = active.max(floor);
        let passed = delta(row)? <= proposed;
        row["active_allowance"] = json!(active);
        row["proposed_allowance"] = json!(proposed);
        row["proposed_passed"] = passed.into();
    }
    Ok(allowances)
}

#[derive(Clone, Copy)]
struct Ratio {
    numerator: i128,
    denominator: u64,
}

impl Ratio {
    fn compare(self, other: Self) -> Ordering {
        let left_sign = self.numerator.signum();
        let right_sign = other.numerator.signum();
        if left_sign != right_sign {
            return left_sign.cmp(&right_sign);
        }
        let order = (self.numerator.unsigned_abs() * u128::from(other.denominator))
            .cmp(&(other.numerator.unsigned_abs() * u128::from(self.denominator)));
        if left_sign < 0 {
            order.reverse()
        } else {
            order
        }
    }
    fn exact(self) -> Value {
        let mut divisor = i128::from(self.denominator);
        let mut remainder = (self.numerator % divisor).abs();
        while remainder != 0 {
            (divisor, remainder) = (remainder, divisor % remainder);
        }
        json!({"numerator": self.numerator / divisor, "denominator": i128::from(self.denominator) / divisor})
    }
}

fn distribution<T>(ordered: &[T], undefined: usize, convert: impl Fn(&T) -> Value) -> Value {
    let select = |index: Option<usize>| {
        index
            .and_then(|index| ordered.get(index))
            .map_or(Value::Null, &convert)
    };
    let nearest = |percentile: usize| {
        if ordered.is_empty() {
            None
        } else {
            Some((percentile * ordered.len()).div_ceil(100) - 1)
        }
    };
    json!({"n": ordered.len(), "undefined": undefined, "min": select(Some(0)), "max": select(ordered.len().checked_sub(1)), "p50": select(nearest(50)), "p95": select(nearest(95))})
}

fn summaries(rows: &[Value]) -> ToolResult<Vec<Value>> {
    let mut groups: BTreeMap<(String, String, Identity, String), Vec<&Value>> = BTreeMap::new();
    for row in rows {
        groups
            .entry((
                row["role"].as_str().ok_or("role missing")?.to_owned(),
                row["variant"].as_str().ok_or("variant missing")?.to_owned(),
                identity(row)?,
                row["metric"].as_str().ok_or("metric missing")?.to_owned(),
            ))
            .or_default()
            .push(row);
    }
    let mut summaries = Vec::with_capacity(groups.len());
    for ((role, variant, (name, id), metric), rows) in groups {
        let mut absolute = rows
            .iter()
            .map(|row| delta(row))
            .collect::<ToolResult<Vec<_>>>()?;
        absolute.sort_unstable();
        let mut relative = Vec::new();
        for row in &rows {
            let denominator = row["baseline"].as_u64().ok_or("baseline missing")?;
            if denominator != 0 {
                relative.push(Ratio {
                    numerator: delta(row)?,
                    denominator,
                });
            }
        }
        relative.sort_by(|left, right| left.compare(*right));
        summaries.push(json!({"role": role, "variant": variant, "function_name": name, "id": id, "metric": metric, "samples": rows.len(), "absolute_delta": distribution(&absolute, 0, |number| json!(number)), "relative_delta": distribution(&relative, rows.len() - relative.len(), |ratio| json!(relative_float(ratio.numerator, ratio.denominator))), "relative_delta_exact": distribution(&relative, rows.len() - relative.len(), |ratio| ratio.exact()), "current_exceedances": rows.iter().filter(|row| row["current_passed"] == false).count(), "proposed_exceedances": rows.iter().filter(|row| row["proposed_passed"] == false).count()}));
    }
    Ok(summaries)
}

pub(super) fn build_report(jobs: &[Value]) -> ToolResult<Value> {
    validate_comparability(jobs)?;
    let mut rows = comparisons(jobs)?;
    let allowances = learn_allowances(&mut rows)?;
    let summaries = summaries(&rows)?;
    let validation_aa: Vec<_> = rows
        .iter()
        .filter(|row| row["role"] == "validation" && row["variant"] == "b")
        .collect();
    let repeatability_failures: Vec<_> = rows
        .iter()
        .filter(|row| row["variant"] == "b" && row["current_passed"] == false)
        .collect();
    let signals: Vec<_> = rows
        .iter()
        .filter(|row| {
            (row["variant"] == "extra_work" || row["variant"] == "extra_alloc")
                && row["metric"] == "Ir"
                && row["function_name"] == "inlay_hints_warm"
                && matches!(row["id"].as_str(), Some("small" | "medium" | "large"))
        })
        .collect();
    let failures: Vec<_> = signals
        .iter()
        .filter(|row| row["role"] == "validation" && row["current_passed"] == true)
        .collect();
    let role_count = |role: &str| jobs.iter().filter(|job| job["role"] == role).count();
    let sufficient = role_count("discovery") == 7
        && role_count("validation") == 3
        && jobs.iter().all(|job| {
            job["samples"]
                .as_array()
                .is_some_and(|samples| samples.len() == 25)
        });
    let observed_pairs: Map<_, _> = jobs
        .iter()
        .map(|job| {
            Ok((
                job["job_id"].as_str().ok_or("job_id missing")?.to_owned(),
                json!(job["samples"].as_array().ok_or("samples missing")?.len() / 5),
            ))
        })
        .collect::<ToolResult<_>>()?;
    let hosts: Map<_, _> = jobs
        .iter()
        .map(|job| {
            Ok((
                job["job_id"].as_str().ok_or("job_id missing")?.to_owned(),
                job.get("host").cloned().unwrap_or_else(|| json!({})),
            ))
        })
        .collect::<ToolResult<_>>()?;
    let mut provenance = Vec::with_capacity(jobs.len());
    for job in jobs {
        let mut record = Map::new();
        for key in [
            "format_version",
            "job_id",
            "role",
            "source_revision",
            "environment",
            "fixture_sha256",
            "harness_sha256",
            "runner_sha256",
            "shard",
            "shards",
            "variants",
        ] {
            record.insert(key.to_owned(), job[key].clone());
        }
        record.insert("benchmark_scope".to_owned(), benchmark_scope(job)?);
        record.insert("execution_order".to_owned(), Value::Array(job["samples"].as_array().ok_or("samples missing")?.iter().map(|sample| json!({"pair": sample["pair"], "variant": sample["variant"], "order": sample["order"]})).collect()));
        provenance.push(Value::Object(record));
    }
    let by_metric: Map<_, _> = METRICS.iter().map(|metric| ((*metric).to_owned(), json!({"samples": validation_aa.iter().filter(|row| row["metric"] == *metric).count(), "current_false_positives": validation_aa.iter().filter(|row| row["metric"] == *metric && row["current_passed"] == false).count(), "proposed_false_positives": validation_aa.iter().filter(|row| row["metric"] == *metric && row["proposed_passed"] == false).count()}))).collect();
    Ok(json!({
        "format_version": 1, "method": "historical", "mode": "proposal-only", "active_gates_changed": false,
        "benchmark_scope": benchmark_scope(jobs.first().ok_or("no calibration jobs")?)?,
        "coverage": {"planned_jobs": {"discovery": 7, "validation": 3}, "planned_pairs_per_job": 5, "observed_jobs": {"discovery": role_count("discovery"), "validation": role_count("validation")}, "observed_pairs": observed_pairs, "predeclared_coverage_met": sufficient, "limitation": if sufficient { "Predeclared coverage collected; finite dependent samples do not establish tail reliability or policy approval." } else { "Insufficient predeclared coverage. No calibrated policy approval or tail reliability claim." }},
        "dependence": "Each pair reuses the same a baseline for b, layout, extra_work and extra_alloc. Comparisons within a pair are dependent; pairs within a job share builds and environment.",
        "learning": "Cache allowance per workload/event is max(active allowance for the comparison baseline, maximum positive discovery A/A delta). Validation, layout, and positive controls never train it. Ir remains unchanged at 2%.",
        "layout": "Diagnostic only; layout is not assumed cost-equivalent A/A noise and does not train allowances.",
        "percentiles": "Nearest-rank empirical p50/p95; not estimates of independent tail reliability.",
        "jobs": provenance, "hosts": hosts, "cache_allowances": allowances, "comparisons": rows, "summaries": summaries,
        "held_out_aa": {"samples": validation_aa.len(), "current_false_positives": validation_aa.iter().filter(|row| row["current_passed"] == false).count(), "proposed_false_positives": validation_aa.iter().filter(|row| row["proposed_passed"] == false).count(), "by_metric": by_metric},
        "control_validation": {
            "passed": sufficient && repeatability_failures.is_empty() && failures.is_empty(),
            "coverage_sufficient": sufficient,
            "repeatability_passed": repeatability_failures.is_empty(),
            "sensitivity_passed": failures.is_empty(),
            "repeatability_failures": repeatability_failures
        },
        "sensitivity": {"passed": failures.is_empty(), "policy": "unchanged Ir 2%", "signals": signals, "validation_failures": failures}
    }))
}

pub(super) fn check_expected_coverage(
    jobs: &[Value],
    discovery: Option<u64>,
    validation: Option<u64>,
    pairs: Option<u64>,
) -> ToolResult<()> {
    for (role, expected) in [("discovery", discovery), ("validation", validation)] {
        if let Some(expected) = expected {
            let observed = u64::try_from(jobs.iter().filter(|job| job["role"] == role).count())?;
            require(
                observed == expected,
                &format!("expected {expected} {role} jobs, found {observed}"),
            )?;
        }
    }
    if let Some(expected) = pairs {
        for job in jobs {
            let observed =
                u64::try_from(job["samples"].as_array().ok_or("samples missing")?.len() / 5)?;
            require(
                observed == expected,
                &format!(
                    "job {}: expected {expected} complete pairs, found {observed}",
                    job["job_id"]
                ),
            )?;
        }
    }
    Ok(())
}

pub(super) fn check_expected_scope(
    jobs: &[Value],
    expected_workloads: Option<u64>,
    require_all_benchmarks: bool,
) -> ToolResult<()> {
    for job in jobs {
        if require_all_benchmarks {
            require(
                job["format_version"] == 2,
                "all benchmarks require v2 analysis and reference_locations evidence",
            )?;
            for variant in VARIANTS {
                benchmark_map(&job["variants"][variant]["benchmarks"])?;
            }
        }
        if let Some(expected) = expected_workloads {
            let observed =
                u64::try_from(workload_identities(&job["samples"][0]["workloads"])?.len())?;
            require(
                observed == expected,
                &format!(
                    "job {}: expected {expected} workloads, found {observed}",
                    job["job_id"]
                ),
            )?;
        }
    }
    Ok(())
}

fn escape(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}
fn text<'a>(value: &'a Value, key: &str) -> ToolResult<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("missing string {key}").into())
}
fn workload_name(row: &Value) -> ToolResult<String> {
    let (name, id) = identity(row)?;
    Ok(id.map_or_else(|| name.clone(), |id| format!("{name} [{id}]")))
}
fn percentage(value: &Value) -> String {
    value.as_f64().map_or_else(
        || "undefined".to_owned(),
        |number| format!("{:+.2}%", number * 100.0),
    )
}

fn render_sensitivity(report: &Value, output: &mut String) -> ToolResult<()> {
    output.push_str("\n## Unchanged 2% Ir positive-control sensitivity\n\n");
    output.push_str(if report["sensitivity"]["passed"] == true {
        "PASS"
    } else {
        "FAIL: one or more validation controls were not detected."
    });
    output.push_str("\n\n| Job | Pair | Control | Workload | Ir delta | Ir relative delta | Detected |\n| --- | ---: | --- | --- | ---: | ---: | --- |\n");
    for row in report["sensitivity"]["signals"]
        .as_array()
        .ok_or("signals missing")?
    {
        let relative = if row["relative_delta"].is_null() {
            "undefined (zero baseline)".to_owned()
        } else {
            percentage(&row["relative_delta"])
        };
        writeln!(
            output,
            "| {} | {} | {} | {} | {:+} | {relative} | {} |",
            escape(text(row, "job_id")?),
            row["pair"],
            text(row, "variant")?,
            escape(text(row, "id")?),
            delta(row)?,
            if row["current_passed"] == true {
                "no"
            } else {
                "yes"
            }
        )?;
    }
    Ok(())
}

pub(super) fn render_markdown(report: &Value) -> ToolResult<String> {
    let mut output = format!(
        "# Performance calibration (proposal-only)\n\nActive comparison limits are unchanged. This report does not approve a calibrated policy.\n\n## Control validation: {}\n\nFull predeclared coverage: {}. Source-identical A/A within active limits: {}. Positive controls detected: {}.\n\nA failed control check makes these measurements unsuitable for accepting a performance comparison. It does not authorize wider limits or a regression exception.\n\n## Coverage and dependence\n\n{}\n\nObserved jobs: {}; pairs: {}.\n\n{}\n\n{}\n\n## Discovery-only proposal\n\n{}\n\n{}\n\n| Workload | Event | Discovery positive A/A floor (events) |\n| --- | --- | ---: |\n",
        if report["control_validation"]["passed"] == true {
            "PASS"
        } else {
            "FAIL"
        },
        report["control_validation"]["coverage_sufficient"],
        report["control_validation"]["repeatability_passed"],
        report["control_validation"]["sensitivity_passed"],
        text(&report["coverage"], "limitation")?,
        report["coverage"]["observed_jobs"],
        report["coverage"]["observed_pairs"],
        text(report, "dependence")?,
        text(report, "percentiles")?,
        text(report, "learning")?,
        text(report, "layout")?
    );
    for row in report["cache_allowances"]
        .as_array()
        .ok_or("allowances missing")?
    {
        writeln!(
            output,
            "| {} | {} | {} |",
            escape(&workload_name(row)?),
            text(row, "metric")?,
            row["discovery_positive_delta_floor"]
        )?;
    }
    output.push_str("\n## Held-out A/A false positives\n\nAll A/A exceedances are retained. With --require-control-validation, an exceedance fails control validation after reports are written; collection evidence is not discarded.\n\n| Event | Comparisons | Current false positives | Proposed false positives |\n| --- | ---: | ---: | ---: |\n");
    for metric in METRICS {
        let values = &report["held_out_aa"]["by_metric"][metric];
        writeln!(
            output,
            "| {metric} | {} | {} | {} |",
            values["samples"],
            values["current_false_positives"],
            values["proposed_false_positives"]
        )?;
    }
    render_sensitivity(report, &mut output)?;
    output.push_str("\n## All A/A and control distributions\n\nRelative deltas retain sign; zero baselines are undefined, but absolute comparisons remain active.\n\n| Role | Variant | Workload | Event | n | Delta min / p50 / p95 / max | Relative p50 / p95 / max | Undefined relative | Current / proposed exceedances |\n| --- | --- | --- | --- | ---: | --- | --- | ---: | --- |\n");
    for row in report["summaries"].as_array().ok_or("summaries missing")? {
        let absolute = &row["absolute_delta"];
        let relative = &row["relative_delta"];
        let deltas = ["min", "p50", "p95", "max"]
            .map(|key| absolute[key].to_string())
            .join(" / ");
        let percentages = ["p50", "p95", "max"]
            .map(|key| percentage(&relative[key]))
            .join(" / ");
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {deltas} | {percentages} | {} | {} / {} |",
            text(row, "role")?,
            text(row, "variant")?,
            escape(&workload_name(row)?),
            text(row, "metric")?,
            row["samples"],
            relative["undefined"],
            row["current_exceedances"],
            row["proposed_exceedances"]
        )?;
    }
    writeln!(
        output,
        "\n## Executed benchmark scope\n\n{}\n\nHistorical v1 evidence covers analysis only; it does not establish reference-location coverage.",
        report["benchmark_scope"]
    )?;
    Ok(output)
}

pub(super) fn run(args: &ReportArgs) -> ToolResult<()> {
    if args.method == super::ReportMethod::PairedMedian105V1 {
        return super::repeated::run(args);
    }
    let jobs = read_datasets(&args.input_root)?;
    check_expected_coverage(
        &jobs,
        args.expected_discovery_jobs,
        args.expected_validation_jobs,
        args.expected_pairs,
    )?;
    let expected_scope =
        check_expected_scope(&jobs, args.expected_workloads, args.require_all_benchmarks)
            .and_then(|()| check_expected_shards(&jobs, args.expected_shards));
    let mut report = build_report(&jobs)?;
    if let Err(error) = &expected_scope {
        report["coverage"]["expectation_error"] = json!(error.to_string());
        report["coverage"]["predeclared_coverage_met"] = json!(false);
        report["control_validation"]["passed"] = json!(false);
        report["control_validation"]["coverage_sufficient"] = json!(false);
    }
    write_json(&args.json_output, &report)?;
    if let Some(parent) = args.markdown_output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.markdown_output, render_markdown(&report)?)?;
    expected_scope?;
    require(
        report["sensitivity"]["passed"] == true,
        "Calibration sensitivity failed: validation work/alloc controls did not exceed unchanged 2% Ir policy.",
    )?;
    require(
        !args.require_control_validation || report["control_validation"]["passed"] == true,
        "Calibration control validation failed: require full coverage, source-identical A/A within active limits, and detected positive controls. Retained reports include the failures.",
    )
}
