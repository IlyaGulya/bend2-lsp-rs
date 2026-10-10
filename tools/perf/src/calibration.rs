//! Calibration collection and proposal-only reporting, `format_version = 1`.
//!
//! `collect --source DIR --work-dir DIR --output-dir DIR --job-id ID
//! --role discovery|validation [--pairs 5]` requires hosted CI.
//! `report INPUT --json-output FILE --markdown-output FILE
//! [--expected-discovery-jobs N] [--expected-validation-jobs N]
//! [--expected-pairs N]` validates retained evidence without measurements.
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
mod integrity;
mod report;
#[cfg(test)]
mod tests;

use crate::ToolResult;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

const VARIANTS: [&str; 5] = ["a", "b", "layout", "extra_work", "extra_alloc"];
const METRICS: [&str; 3] = ["Ir", "I1mr", "ILmr"];
const ROLES: [&str; 2] = ["discovery", "validation"];
const PROBE: &str = "calibration_layout_probe";

type Identity = (String, Option<String>);

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
}

#[derive(Parser)]
struct ReportArgs {
    input_root: PathBuf,
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
}

fn positive_integer(value: &str) -> Result<u64, String> {
    let number = value.parse::<u64>().map_err(|error| error.to_string())?;
    if number == 0 {
        return Err("expected a positive integer".into());
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

fn require(condition: bool, message: &str) -> ToolResult<()> {
    if !condition {
        return Err(message.to_owned().into());
    }
    Ok(())
}

fn sha256_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn baseline_name(job_id: &str, pair: u64, variant: &str) -> String {
    sha256_bytes(format!("{job_id}:{pair}:{variant}").as_bytes())
}
