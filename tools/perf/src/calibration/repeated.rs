//! Report-only fixed experiment; never an ordinary performance admission path.
use super::{ReportArgs, report, require};
use crate::{
    ToolResult,
    common::write_json,
    repeated::{METHOD, PAIRS, comparisons},
};
use serde_json::{Value, json};
use std::{fmt::Write as _, fs};

fn text<'a>(value: &'a Value, key: &str) -> ToolResult<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("missing {key}").into())
}

fn balanced(job: &Value) -> bool {
    let mut positions = [[0_usize; 5]; 5];
    let Some(samples) = job["samples"].as_array() else {
        return false;
    };
    for sample in samples {
        let Some(variant) = super::VARIANTS.iter().position(|v| sample["variant"] == *v) else {
            return false;
        };
        let Some(position) = sample["order"]
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v < 5)
        else {
            return false;
        };
        positions[variant][position] += 1;
    }
    positions.iter().flatten().all(|count| *count == PAIRS / 5)
}

fn build(jobs: &[Value], args: &ReportArgs) -> ToolResult<Value> {
    // Reuse the authoritative historical provenance and raw-policy diagnostics.
    // Historical conclusions are explicitly nested, not experiment verdicts.
    let mut historical = report::build_report(jobs)?;
    let raw = historical["comparisons"].take();
    let repeated = comparisons(raw.as_array().ok_or("comparisons missing")?)?;
    let expected_ids: Vec<_> = ["discovery", "validation"]
        .into_iter()
        .flat_map(|role| (1..=3).map(move |i| (format!("{role}-{i:02}"), role)))
        .collect();
    let design = jobs.len() == 6
        && expected_ids.iter().all(|(id, role)| {
            jobs.iter()
                .any(|j| j["job_id"] == *id && j["role"] == *role && balanced(j))
        });
    let expected = report::check_expected_coverage(
        jobs,
        args.expected_discovery_jobs,
        args.expected_validation_jobs,
        args.expected_pairs,
    )
    .and_then(|()| {
        report::check_expected_scope(jobs, args.expected_workloads, args.require_all_benchmarks)
    });
    let coverage = design
        && expected.is_ok()
        && args.expected_discovery_jobs.is_none_or(|n| n == 3)
        && args.expected_validation_jobs.is_none_or(|n| n == 3)
        && args.expected_pairs.is_none_or(|n| n == 105);
    let aa: Vec<_> = repeated.iter().filter(|r| r["variant"] == "b").collect();
    let failures: Vec<_> = aa
        .iter()
        .filter(|r| r["classification"] != "passed")
        .collect();
    let controls: Vec<_> = repeated
        .iter()
        .filter(|r| {
            r["role"] == "validation"
                && matches!(r["variant"].as_str(), Some("extra_work" | "extra_alloc"))
                && r["function_name"] == "inlay_hints_warm"
                && matches!(r["id"].as_str(), Some("small" | "medium" | "large"))
                && r["metric"] == "Ir"
        })
        .collect();
    let missed: Vec<_> = controls
        .iter()
        .filter(|r| r["classification"] != "regression")
        .collect();
    let repeatability = !aa.is_empty() && failures.is_empty();
    let sensitivity = controls.len() == 18 && missed.is_empty();
    let provenance = historical["jobs"].take();
    let hosts = historical["hosts"].take();
    Ok(json!({
        "format_version": 1, "method": METHOD, "mode": "report-only", "active_gates_changed": false, "evidence_verified": true,
        "benchmark_scope": historical["benchmark_scope"],
        "coverage": {"planned_jobs": {"discovery": 3, "validation": 3}, "planned_pairs_per_job": 105, "predeclared_coverage_met": coverage, "observed_jobs": historical["coverage"]["observed_jobs"], "observed_pairs": historical["coverage"]["observed_pairs"], "balanced_execution_order": jobs.iter().all(balanced), "expectation_error": expected.err().map(|e| e.to_string())},
        "control_validation": {"passed": coverage && repeatability && sensitivity, "classification": if !coverage { "insufficient_evidence" } else if repeatability && sensitivity { "passed" } else { "failed" }, "coverage_sufficient": coverage, "repeatability_passed": repeatability, "sensitivity_passed": sensitivity, "repeatability_failures": failures, "sensitivity_failures": missed},
        "assumptions": "Conditional independent fresh-process samples within each job under randomized balanced execution order. No pooling across jobs, hosts, or builds. Reused a baselines make within-pair comparisons dependent. Bounds are one-sided marginal 95% bounds, not simultaneous or familywise claims. All upper bounds passing is an intersection-union criterion, conditional on sampling assumptions. This estimates typical median process change, not p95, tails, or editor latency.",
        "boundary_semantics": "Exact residual (candidate-baseline-active_allowance)/max(baseline,1). Ties are retained. Upper <= 0 passes; lower > 0 is regression; otherwise inconclusive. Median bounds are conservative for atoms/ties. Exactly 105 pairs; no adaptive stopping or retries.",
        "allowances": "Unchanged authoritative Ir floor 2%; cache ceil 3%, minimum 3. No learned allowances apply. Layout is diagnostic only, not assumed cost-equivalent.",
        "repeated_comparisons": repeated, "comparisons": raw,
        "jobs": provenance, "hosts": hosts,
        "historical_diagnostics": historical
    }))
}

fn markdown(report: &Value) -> ToolResult<String> {
    let mut output = format!(
        "# Performance calibration: {METHOD} (report-only)\n\nActive gates changed: false. This experiment never grants performance admission.\n\n## Control validation\n\n{}\n\n## Coverage\n\n{}\n\n",
        report["control_validation"], report["coverage"]
    );
    if report["evidence_verified"] != true {
        writeln!(output, "Unverified evidence: {}", report["evidence_error"])?;
        return Ok(output);
    }
    writeln!(
        output,
        "## Executed benchmark scope\n\n{}\n\nHistorical v1 evidence covers analysis only; it does not establish reference-location coverage.\n",
        report["benchmark_scope"]
    )?;
    writeln!(
        output,
        "{}\n\n{}\n\n{}\n\n## Per-job exact median residual bounds\n\n| Job | Variant | Workload | Event | n | Median | Lower | Upper | Classification |\n| --- | --- | --- | --- | ---: | --- | --- | --- | --- |",
        text(report, "assumptions")?,
        text(report, "boundary_semantics")?,
        text(report, "allowances")?
    )?;
    for row in report["repeated_comparisons"]
        .as_array()
        .ok_or("comparisons missing")?
    {
        let safe = |key| row[key].to_string().replace('|', "\\|").replace('\n', " ");
        writeln!(
            output,
            "| {} | {} | {} / {} | {} | {} | {} | {} | {} | {} |",
            safe("job_id"),
            safe("variant"),
            safe("function_name"),
            safe("id"),
            row["metric"],
            row["samples"],
            row["median"],
            row["lower"],
            row["upper"],
            row["classification"]
        )?;
    }
    output.push_str("\nAll raw pairs, unchanged single-pair verdicts, exact confidence fractions/ranks, and historical proposal diagnostics are retained in JSON. Historical diagnostics do not determine this experiment's verdict; learned allowances never apply.\n");
    Ok(output)
}

pub(super) fn run(args: &ReportArgs) -> ToolResult<()> {
    let result = report::read_datasets(&args.input_root).and_then(|jobs| build(&jobs, args));
    let report = match result {
        Ok(report) => report,
        Err(error) => {
            json!({"format_version": 1, "method": METHOD, "mode": "report-only", "active_gates_changed": false, "evidence_verified": false, "evidence_error": error.to_string(), "coverage": {"planned_jobs": {"discovery": 3, "validation": 3}, "planned_pairs_per_job": 105, "predeclared_coverage_met": false}, "control_validation": {"passed": false, "classification": "insufficient_evidence", "coverage_sufficient": false, "repeatability_passed": false, "sensitivity_passed": false}})
        }
    };
    write_json(&args.json_output, &report)?;
    if let Some(parent) = args.markdown_output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.markdown_output, markdown(&report)?)?;
    require(
        report["control_validation"]["passed"] == true,
        "Calibration control validation failed: paired-median-105-v1 requires verified balanced 3/3/105 coverage, every A/A passed, and every held-out positive control classified regression. Reports retained.",
    )
}
