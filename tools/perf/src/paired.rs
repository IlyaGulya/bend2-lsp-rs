//! Fixed-series source comparisons; calibration remains a separate control experiment.
mod collector;
pub(crate) mod report;

use crate::{ToolResult, callgrind, common::write_json, policy::ExitError, repeated::METHOD};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};

#[derive(Parser)]
#[command(no_binary_name = true)]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Collect exactly 105 fresh source pairs on the qualified hosted platform.
    Collect(CollectArgs),
    /// Revalidate retained paired evidence and report exact fixed-series bounds.
    Report(ReportArgs),
}

#[derive(clap::Args)]
struct CollectArgs {
    #[arg(long)]
    baseline_source: PathBuf,
    #[arg(long)]
    candidate_source: PathBuf,
    #[arg(long)]
    work_dir: PathBuf,
    #[arg(long)]
    output_dir: PathBuf,
}

#[derive(clap::Args)]
struct ReportArgs {
    root: PathBuf,
    #[arg(long)]
    json_output: PathBuf,
    #[arg(long)]
    markdown_output: PathBuf,
}

fn report_outputs(args: &ReportArgs) -> ToolResult<(Value, String, u8)> {
    let root = args.root.canonicalize()?;
    let manifest = callgrind::strict_json(&fs::read_to_string(root.join("artifact-manifest.json"))?)?;
    let verdict = report::build(&root, &manifest)?;
    let status = match verdict["status"].as_str() {
        Some("complete") => 0,
        Some("regression") => 1,
        Some("inconclusive") => 3,
        _ => return Err("Unknown paired measurement outcome".into()),
    };
    let markdown = report::markdown(&verdict)?;
    Ok((verdict, markdown, status))
}

fn run_report(args: &ReportArgs) -> ToolResult<()> {
    let (verdict, markdown, status) = match report_outputs(args) {
        Ok(outputs) => outputs,
        Err(error) => (
            json!({"format_version": 2, "kind": "callgrind-policy", "method": METHOD,
                "status": "error", "error": error.to_string()}),
            format!("# Paired Callgrind measurement failed\n\n{error}\n"),
            2,
        ),
    };
    write_json(&args.json_output, &verdict)?;
    if let Some(parent) = args.markdown_output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.markdown_output, markdown)?;
    if status == 0 {
        Ok(())
    } else {
        let message = verdict["error"].as_str().map_or_else(
            || format!("Paired Callgrind policy: {}", verdict["status"]),
            str::to_owned,
        );
        Err(Box::new(ExitError::new(status, message)))
    }
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    match Arguments::try_parse_from(args)?.command {
        Command::Collect(args) => {
            crate::common::require_ci()?;
            collector::collect(&args)
        }
        Command::Report(args) => run_report(&args),
    }
}
