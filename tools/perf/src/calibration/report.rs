use super::{
    Identity, METRICS, PROBE, ROLES, ReportArgs, VARIANTS, baseline_name,
    integrity::{identity, parse_measurement, signals, strict_json},
    require,
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
fn digest(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        text.len() == 64
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn validate_metadata(data: &Value) -> ToolResult<()> {
    require(data.is_object(), "dataset must be an object")?;
    require(
        data["format_version"].as_u64() == Some(1),
        "format_version must be integer 1",
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
    for field in ["fixture_sha256", "harness_sha256"] {
        require(
            digest(&data[field]),
            &format!("{field} must be a lowercase SHA256"),
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
    for (name, variant) in variants {
        require(variant.is_object(), "variant must be an object")?;
        for field in ["source_sha256", "binary_sha256"] {
            require(
                digest(&variant[field]),
                "variant source/binary hash must be a lowercase SHA256",
            )?;
        }
        require(
            nonempty(&variant["executable"]),
            "variant executable must be nonempty",
        )?;
        let probe = &variant["layout_probe"];
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
                .is_some_and(|size| size >= if name == "layout" { 1024 } else { 1 }),
            "variant probe size is invalid",
        )?;
        require(
            variant
                .get("layout_probe_collected")
                .is_none_or(|value| value == false),
            "variant layout probe must have no collected execution",
        )?;
    }
    require(
        variants["a"]["source_sha256"] == variants["b"]["source_sha256"],
        "A/A source hashes differ",
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

pub(super) fn validate_job(data: &Value, root: &Path) -> ToolResult<()> {
    validate_metadata(data)?;
    let samples = data["samples"]
        .as_array()
        .filter(|samples| !samples.is_empty())
        .ok_or("samples must be a nonempty list")?;
    let mut seen = BTreeSet::new();
    let mut orders: BTreeMap<u64, BTreeSet<u64>> = BTreeMap::new();
    let mut identities = None;
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
        let stdout = evidence_path(root, sample, "raw_stdout")?;
        evidence_path(root, sample, "raw_stderr")?;
        let current = workload_identities(&sample["workloads"])?;
        if let Some(previous) = &identities {
            require(
                &current == previous,
                "workload identities differ between samples",
            )?;
        }
        let baseline = baseline_name(
            data["job_id"].as_str().ok_or("job_id missing")?,
            pair,
            variant,
        );
        let executable = Path::new(
            data["variants"][variant]["executable"]
                .as_str()
                .ok_or("executable missing")?,
        );
        let (measured, _) = parse_measurement(
            &fs::read_to_string(stdout)?,
            &baseline,
            executable,
            &current,
        )?;
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
        )?;
        identities = Some(current);
    }
    require(
        identities
            .as_ref()
            .is_some_and(|identities| signals().is_subset(identities)),
        "missing small/medium/large inlay_hints_warm controls",
    )?;
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

pub(super) fn read_datasets(root: &Path) -> ToolResult<Vec<Value>> {
    require(root.is_dir(), "input root must be a directory")?;
    let mut paths = Vec::new();
    dataset_paths(root, &root.canonicalize()?, &mut paths)?;
    paths.sort();
    require(!paths.is_empty(), "no data.json datasets found")?;
    let mut jobs = Vec::with_capacity(paths.len());
    for path in paths {
        let job = strict_json(&fs::read_to_string(&path)?)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let parent = path.parent().ok_or("dataset directory missing")?;
        validate_job(&job, parent).map_err(|error| format!("{}: {error}", path.display()))?;
        jobs.push(job);
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
    for job in &jobs[1..] {
        for field in [
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
            "job_id",
            "role",
            "source_revision",
            "environment",
            "fixture_sha256",
            "harness_sha256",
            "variants",
        ] {
            record.insert(key.to_owned(), job[key].clone());
        }
        record.insert("execution_order".to_owned(), Value::Array(job["samples"].as_array().ok_or("samples missing")?.iter().map(|sample| json!({"pair": sample["pair"], "variant": sample["variant"], "order": sample["order"]})).collect()));
        provenance.push(Value::Object(record));
    }
    let by_metric: Map<_, _> = METRICS.iter().map(|metric| ((*metric).to_owned(), json!({"samples": validation_aa.iter().filter(|row| row["metric"] == *metric).count(), "current_false_positives": validation_aa.iter().filter(|row| row["metric"] == *metric && row["current_passed"] == false).count(), "proposed_false_positives": validation_aa.iter().filter(|row| row["metric"] == *metric && row["proposed_passed"] == false).count()}))).collect();
    Ok(json!({
        "format_version": 1, "mode": "proposal-only", "active_gates_changed": false,
        "coverage": {"planned_jobs": {"discovery": 7, "validation": 3}, "planned_pairs_per_job": 5, "observed_jobs": {"discovery": role_count("discovery"), "validation": role_count("validation")}, "observed_pairs": observed_pairs, "predeclared_coverage_met": sufficient, "limitation": if sufficient { "Predeclared coverage collected; finite dependent samples do not establish tail reliability or policy approval." } else { "Insufficient predeclared coverage. No calibrated policy approval or tail reliability claim." }},
        "dependence": "Each pair reuses the same a baseline for b, layout, extra_work and extra_alloc. Comparisons within a pair are dependent; pairs within a job share builds and environment.",
        "learning": "Cache allowance per workload/event is max(active allowance for the comparison baseline, maximum positive discovery A/A delta). Validation, layout, and positive controls never train it. Ir remains unchanged at 2%.",
        "layout": "Diagnostic only; layout is not assumed cost-equivalent A/A noise and does not train allowances.",
        "percentiles": "Nearest-rank empirical p50/p95; not estimates of independent tail reliability.",
        "jobs": provenance, "hosts": hosts, "cache_allowances": allowances, "comparisons": rows, "summaries": summaries,
        "held_out_aa": {"samples": validation_aa.len(), "current_false_positives": validation_aa.iter().filter(|row| row["current_passed"] == false).count(), "proposed_false_positives": validation_aa.iter().filter(|row| row["proposed_passed"] == false).count(), "by_metric": by_metric},
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

pub(super) fn render_markdown(report: &Value) -> ToolResult<String> {
    let mut output = format!(
        "# Performance calibration (proposal-only)\n\nActive blocking gates are unchanged. This report does not approve a calibrated policy.\n\n## Coverage and dependence\n\n{}\n\nObserved jobs: {}; pairs: {}.\n\n{}\n\n{}\n\n## Discovery-only proposal\n\n{}\n\n{}\n\n| Workload | Event | Discovery positive A/A floor (events) |\n| --- | --- | ---: |\n",
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
    output.push_str("\n## Held-out A/A false positives\n\nCache A/A noise is reported, never suppressed, and does not fail collection or this report.\n\n| Event | Comparisons | Current false positives | Proposed false positives |\n| --- | ---: | ---: | ---: |\n");
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
    Ok(output)
}

pub(super) fn run(args: &ReportArgs) -> ToolResult<()> {
    let jobs = read_datasets(&args.input_root)?;
    check_expected_coverage(
        &jobs,
        args.expected_discovery_jobs,
        args.expected_validation_jobs,
        args.expected_pairs,
    )?;
    let report = build_report(&jobs)?;
    write_json(&args.json_output, &report)?;
    if let Some(parent) = args.markdown_output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.markdown_output, render_markdown(&report)?)?;
    require(
        report["sensitivity"]["passed"] == true,
        "Calibration sensitivity failed: validation work/alloc controls did not exceed unchanged 2% Ir policy.",
    )
}
