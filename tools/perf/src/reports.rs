use crate::ToolResult;
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use serde_json::{Map, Number, Value, json};
use std::{collections::BTreeMap, fmt::Write as _, fs, path::PathBuf, str::FromStr};

const WORKLOADS: [&str; 7] = [
    "hover_warm",
    "definition_warm",
    "completion_warm",
    "open_to_hover_large",
    "edit_to_hover_large",
    "hover_during_large_edit",
    "hover_during_large_open",
];
const PAIRED_METADATA: [&str; 6] = [
    "platform", "machine", "harness", "rounds", "samples", "warmup",
];

#[derive(Parser)]
#[command(no_binary_name = true)]
struct ReportsCli {
    #[command(subcommand)]
    command: ReportCommand,
}

#[derive(Subcommand)]
enum ReportCommand {
    /// Validate paired LSP measurements and report changes without thresholds.
    Latency(LatencyArgs),
}

#[derive(Args)]
struct LatencyArgs {
    baseline_json: PathBuf,
    candidate_json: PathBuf,
    #[arg(long)]
    json_output: PathBuf,
    #[arg(long)]
    markdown_output: PathBuf,
}

#[derive(Deserialize)]
struct Measurement {
    format_version: u64,
    workload_digest: String,
    metadata: Map<String, Value>,
    workloads: BTreeMap<String, Workload>,
}

#[derive(Deserialize)]
struct Workload {
    rounds_ns: Vec<Vec<u64>>,
}

#[derive(Clone, Copy)]
struct Percentiles {
    p50_ns: u64,
    p95_ns: u64,
}

struct Summary {
    rounds: Vec<Percentiles>,
    // Doubled nanoseconds preserve exact half-nanosecond even-round medians.
    p50_twice: i128,
    p95_twice: i128,
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    let cli = match ReportsCli::try_parse_from(args) {
        Ok(cli) => cli,
        Err(error) if !error.use_stderr() => {
            error.print()?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let ReportCommand::Latency(args) = cli.command;
    report_latency(&args)
        .map_err(|error| format!("error: cannot compare LSP latency measurements: {error}").into())
}

fn report_latency(args: &LatencyArgs) -> ToolResult<()> {
    let mut baseline: Measurement = serde_json::from_slice(&fs::read(&args.baseline_json)?)
        .map_err(|error| format!("baseline: {error}"))?;
    let mut candidate: Measurement = serde_json::from_slice(&fs::read(&args.candidate_json)?)
        .map_err(|error| format!("candidate: {error}"))?;
    validate_measurement(&baseline, "baseline")?;
    validate_measurement(&candidate, "candidate")?;
    if baseline.workload_digest != candidate.workload_digest {
        return Err("baseline and candidate workload_digest differ".into());
    }
    for field in PAIRED_METADATA {
        if baseline.metadata.get(field) != candidate.metadata.get(field) {
            return Err(format!("baseline and candidate metadata.{field} differ").into());
        }
    }
    let mut workloads = Map::new();
    let mut markdown = String::from(
        "# End-to-end LSP latency\n\n\
         Report-only: numerical latency changes never fail CI; invalid or incomparable measurements do.\n\n\
         Times are milliseconds. Each round uses nearest-rank p50/p95; aggregates are the median of round percentiles, not pooled requests. Deltas are candidate minus baseline. Paired round deltas retain collection order, including alternating baseline/candidate execution.\n\n\
         | Workload | Baseline p50 (ms) | Candidate p50 (ms) | p50 change | Baseline p95 (ms) | Candidate p95 (ms) | p95 change |\n\
         | --- | ---: | ---: | ---: | ---: | ---: | ---: |\n",
    );
    for name in WORKLOADS {
        let left = summarize(
            baseline
                .workloads
                .get_mut(name)
                .ok_or("missing baseline workload")?,
        )?;
        let right = summarize(
            candidate
                .workloads
                .get_mut(name)
                .ok_or("missing candidate workload")?,
        )?;
        let mut changes = delta(
            left.p50_twice,
            left.p95_twice,
            right.p50_twice,
            right.p95_twice,
        )?;
        let round_changes: Vec<Value> = left
            .rounds
            .iter()
            .zip(&right.rounds)
            .map(|(a, b)| {
                delta(
                    i128::from(a.p50_ns) * 2,
                    i128::from(a.p95_ns) * 2,
                    i128::from(b.p50_ns) * 2,
                    i128::from(b.p95_ns) * 2,
                )
                .map(Value::Object)
            })
            .collect::<ToolResult<_>>()?;
        write_markdown_row(&mut markdown, name, &left, &right, &changes)?;
        changes.insert("rounds".into(), Value::Array(round_changes));
        workloads.insert(
            name.into(),
            json!({
                "baseline": summary_json(&left)?,
                "candidate": summary_json(&right)?,
                "delta": changes,
            }),
        );
    }
    let report = json!({
        "format_version": 1,
        "mode": "report-only",
        "workload_digest": baseline.workload_digest,
        "baseline_metadata": baseline.metadata,
        "candidate_metadata": candidate.metadata,
        "workloads": workloads,
    });
    // Finish validation and rendering before touching either output.
    let mut json_text = serde_json::to_string_pretty(&report)?;
    json_text.push('\n');
    fs::write(&args.json_output, json_text)?;
    fs::write(&args.markdown_output, markdown)?;
    Ok(())
}

fn lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn metadata_count(
    measurement: &Measurement,
    source: &str,
    field: &str,
    minimum: u64,
) -> ToolResult<u64> {
    measurement
        .metadata
        .get(field)
        .and_then(Value::as_u64)
        .filter(|value| *value >= minimum)
        .ok_or_else(|| format!("{source}: metadata.{field} must be an integer >= {minimum}").into())
}

fn validate_measurement(measurement: &Measurement, source: &str) -> ToolResult<()> {
    if measurement.format_version != 1 {
        return Err(format!("{source}: format_version must be integer 1").into());
    }
    if measurement
        .metadata
        .get("collection_status")
        .is_some_and(|status| status != "complete")
    {
        return Err(format!("{source}: collection did not complete successfully").into());
    }
    if !lowercase_sha256(&measurement.workload_digest) {
        return Err(format!("{source}: workload_digest must be a lowercase SHA256").into());
    }
    if !measurement
        .metadata
        .get("binary_sha256")
        .and_then(Value::as_str)
        .is_some_and(lowercase_sha256)
    {
        return Err(format!("{source}: metadata.binary_sha256 must be a lowercase SHA256").into());
    }
    for field in ["platform", "machine", "harness"] {
        if measurement
            .metadata
            .get(field)
            .and_then(Value::as_str)
            .is_none_or(|value| value.trim().is_empty())
        {
            return Err(format!("{source}: metadata.{field} must be a nonempty string").into());
        }
    }
    let rounds = metadata_count(measurement, source, "rounds", 1)?;
    let samples = metadata_count(measurement, source, "samples", 1)?;
    metadata_count(measurement, source, "warmup", 0)?;
    if measurement.workloads.len() != WORKLOADS.len()
        || WORKLOADS
            .iter()
            .any(|name| !measurement.workloads.contains_key(*name))
    {
        return Err(
            format!("{source}: workloads must contain exactly the seven supported IDs").into(),
        );
    }
    for (name, workload) in &measurement.workloads {
        if u64::try_from(workload.rounds_ns.len())? != rounds {
            return Err(format!("{source}: {name}.rounds_ns must match metadata.rounds").into());
        }
        for (index, values) in workload.rounds_ns.iter().enumerate() {
            if u64::try_from(values.len())? != samples {
                return Err(format!(
                    "{source}: {name} round {} must match metadata.samples",
                    index + 1
                )
                .into());
            }
            if values.contains(&0) {
                return Err(format!(
                    "{source}: {name} round {} samples must be positive integer nanoseconds",
                    index + 1
                )
                .into());
            }
        }
    }
    Ok(())
}

fn percentile_index(count: usize, percentile: usize) -> usize {
    // ceil(percentile * count / 100) - 1 without overflowing usize.
    count / 100 * percentile + (count % 100 * percentile).div_ceil(100) - 1
}

fn median_twice(values: &mut [u64]) -> i128 {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        i128::from(values[middle - 1]) + i128::from(values[middle])
    } else {
        i128::from(values[middle]) * 2
    }
}

fn summarize(workload: &mut Workload) -> ToolResult<Summary> {
    let mut rounds = Vec::with_capacity(workload.rounds_ns.len());
    for samples in &mut workload.rounds_ns {
        samples.sort_unstable();
        rounds.push(Percentiles {
            p50_ns: *samples
                .get(percentile_index(samples.len(), 50))
                .ok_or("missing p50 sample")?,
            p95_ns: *samples
                .get(percentile_index(samples.len(), 95))
                .ok_or("missing p95 sample")?,
        });
    }
    let mut values: Vec<u64> = rounds.iter().map(|round| round.p50_ns).collect();
    let p50_twice = median_twice(&mut values);
    for (value, round) in values.iter_mut().zip(&rounds) {
        *value = round.p95_ns;
    }
    let p95_twice = median_twice(&mut values);
    Ok(Summary {
        rounds,
        p50_twice,
        p95_twice,
    })
}

fn exact_nanoseconds(twice: i128) -> ToolResult<Number> {
    let text = if twice % 2 == 0 {
        (twice / 2).to_string()
    } else {
        let sign = if twice < 0 { "-" } else { "" };
        format!("{sign}{}.5", twice.abs() / 2)
    };
    Ok(Number::from_str(&text)?)
}

fn floating_nanoseconds(twice: i128) -> ToolResult<f64> {
    exact_nanoseconds(twice)?
        .as_f64()
        .ok_or_else(|| "nanoseconds cannot be represented as a finite report value".into())
}

fn delta(
    left50: i128,
    left95: i128,
    right50: i128,
    right95: i128,
) -> ToolResult<Map<String, Value>> {
    let mut changes = Map::new();
    for (percentile, left, right) in [(50, left50, right50), (95, left95, right95)] {
        let difference = right - left;
        changes.insert(
            format!("p{percentile}_ns"),
            Value::Number(exact_nanoseconds(difference)?),
        );
        changes.insert(
            format!("p{percentile}_percent"),
            json!(floating_nanoseconds(difference)? / floating_nanoseconds(left)? * 100.0),
        );
    }
    Ok(changes)
}

fn summary_json(summary: &Summary) -> ToolResult<Value> {
    let rounds: Vec<Value> = summary
        .rounds
        .iter()
        .map(|round| json!({"p50_ns": round.p50_ns, "p95_ns": round.p95_ns}))
        .collect();
    Ok(json!({
        "rounds": rounds,
        "p50_ns": exact_nanoseconds(summary.p50_twice)?,
        "p95_ns": exact_nanoseconds(summary.p95_twice)?,
    }))
}

fn write_markdown_row(
    markdown: &mut String,
    name: &str,
    left: &Summary,
    right: &Summary,
    changes: &Map<String, Value>,
) -> ToolResult<()> {
    let percent = |key| {
        changes
            .get(key)
            .and_then(Value::as_f64)
            .ok_or_else(|| format!("missing {key} report change"))
    };
    writeln!(
        markdown,
        "| {name} | {:.3} | {:.3} | {:+.2}% | {:.3} | {:.3} | {:+.2}% |",
        floating_nanoseconds(left.p50_twice)? / 1_000_000.0,
        floating_nanoseconds(right.p50_twice)? / 1_000_000.0,
        percent("p50_percent")?,
        floating_nanoseconds(left.p95_twice)? / 1_000_000.0,
        floating_nanoseconds(right.p95_twice)? / 1_000_000.0,
        percent("p95_percent")?,
    )?;
    Ok(())
}
