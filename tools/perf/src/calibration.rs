//! Calibration collection and proposal-only reporting, `format_version = 1`.
//!
//! `collect --source DIR --work-dir DIR --output-dir DIR --job-id ID
//! --role discovery|validation [--pairs 5] [--shard-index I --shard-count N]`
//! requires hosted CI. One logical job may be split across deterministic
//! physical shards that own whole frozen `function_name` units and all of their
//! case IDs; every workload series of a logical job still comes from exactly one
//! physical runner.
//! `report INPUT --json-output FILE --markdown-output FILE
//! [--expected-discovery-jobs N] [--expected-validation-jobs N]
//! [--expected-pairs N] [--expected-shards N]` validates retained evidence
//! without measurements, recomposing complete shard sets before comparing.
//!
//! Collection retains independent source manifests, toolchain/cache comparability,
//! host metadata, per-variant source/binary hashes and nm probe records, complete
//! five-variant pair samples, execution positions, raw stdout/stderr and verified
//! Callgrind profiles. Reports preserve this provenance and exact signed deltas,
//! learn cache floors only from discovery A/A, and leave active gates untouched.
//!
//! Harness transforms canonicalize LF/CRLF source to LF before exact anchor
//! matching; mixed-newline duplicates and non-newline anchor drift still fail.

mod collector;
mod repeated;
mod report;
mod shard;
#[cfg(test)]
mod tests;

use crate::{
    ToolResult,
    callgrind::{self, BENCHMARKS, Identity, METRICS, PROBE, baseline_name, require},
};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

const VARIANTS: [&str; 5] = ["a", "b", "layout", "extra_work", "extra_alloc"];
const ROLES: [&str; 2] = ["discovery", "validation"];

#[derive(Parser)]
#[command(no_binary_name = true)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Collect independently built, report-only calibration pairs in hosted CI.
    Collect(CollectArgs),
    /// Validate retained evidence and report discovery-only proposals locally or in CI.
    Report(ReportArgs),
}

#[derive(Parser)]
struct CollectArgs {
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    work_dir: PathBuf,
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long, value_parser = nonempty_argument)]
    job_id: String,
    #[arg(long, value_parser = ["discovery", "validation"])]
    role: String,
    #[arg(long, default_value = "5", value_parser = positive_integer)]
    pairs: u64,
    /// Deterministic physical shard of the frozen workload manifest (default 0).
    #[arg(long, default_value = "0", value_parser = nonnegative_integer)]
    shard_index: u64,
    /// Number of deterministic physical shards for this logical job (1 = unsharded).
    #[arg(long, default_value = "1", value_parser = shard_count)]
    shard_count: u64,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum ReportMethod {
    #[default]
    Historical,
    #[value(name = "paired-median-105-v1")]
    PairedMedian105V1,
}

#[derive(Parser)]
struct ReportArgs {
    input_root: PathBuf,
    #[arg(long, value_enum, default_value = "historical")]
    method: ReportMethod,
    #[arg(long)]
    json_output: PathBuf,
    #[arg(long)]
    markdown_output: PathBuf,
    #[arg(long, value_parser = positive_integer)]
    expected_discovery_jobs: Option<u64>,
    #[arg(long, value_parser = positive_integer)]
    expected_validation_jobs: Option<u64>,
    #[arg(long, value_parser = positive_integer)]
    expected_pairs: Option<u64>,
    /// Require this many complete physical shards in every logical job.
    #[arg(long, value_parser = positive_integer)]
    expected_shards: Option<u64>,
    /// Require this many unique executed workloads in every job.
    #[arg(long, value_parser = positive_integer)]
    expected_workloads: Option<u64>,
    /// Require source-bound analysis and `reference_locations` evidence.
    #[arg(long)]
    require_all_benchmarks: bool,
    /// Reject incomplete coverage, source-identical A/A failures, or missed positive controls.
    #[arg(long)]
    require_control_validation: bool,
}

fn positive_integer(value: &str) -> Result<u64, String> {
    let number = value.parse::<u64>().map_err(|error| error.to_string())?;
    if number == 0 {
        return Err("expected a positive integer".into());
    }
    Ok(number)
}

fn nonnegative_integer(value: &str) -> Result<u64, String> {
    value.parse::<u64>().map_err(|error| error.to_string())
}

fn shard_count(value: &str) -> Result<u64, String> {
    let number = positive_integer(value)?;
    if number > shard::MAX_COUNT {
        return Err(format!("expected at most {} shards", shard::MAX_COUNT));
    }
    Ok(number)
}

fn nonempty_argument(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        return Err("expected a nonempty job ID".into());
    }
    Ok(value.to_owned())
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    match Arguments::try_parse_from(args)?.command {
        Command::Collect(args) => {
            crate::common::require_ci()?;
            collector::collect(&args)
        }
        Command::Report(args) => report::run(&args),
    }
}
