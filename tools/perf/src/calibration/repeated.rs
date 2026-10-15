//! Report-only fixed experiment; never an ordinary performance admission path.
use super::{Identity, ReportArgs, report, require};
use crate::{ToolResult, common::write_json, policy};
use serde_json::{Value, json};
use std::{cmp::Ordering, collections::BTreeMap, fmt::Write as _, fs};

const PAIRS: usize = 105;
const METHOD: &str = "paired-median-105-v1";

#[derive(Clone, Copy)]
struct Residual {
    numerator: i128,
    denominator: u64,
}

impl Residual {
    fn value(self) -> Value {
        json!({"numerator": self.numerator, "denominator": self.denominator})
    }

    fn compare(self, other: Self) -> Ordering {
        match self.numerator.signum().cmp(&other.numerator.signum()) {
            Ordering::Equal => {
                let order = unsigned_ratio_cmp(self.numerator.unsigned_abs(), u128::from(self.denominator), other.numerator.unsigned_abs(), u128::from(other.denominator));
                if self.numerator < 0 { order.reverse() } else { order }
            }
            order => order,
        }
    }
}

// Continued-fraction comparison avoids cross-products: a residual magnitude can
// exceed u64::MAX, so even u128 cross multiplication is not generally safe.
fn unsigned_ratio_cmp(mut a: u128, mut b: u128, mut c: u128, mut d: u128) -> Ordering {
    let mut reversed = false;
    loop {
        let order = (a / b).cmp(&(c / d));
        if order != Ordering::Equal {
            return if reversed { order.reverse() } else { order };
        }
        let (r, s) = (a % b, c % d);
        if r == 0 || s == 0 {
            let order = r.cmp(&s);
            return if reversed { order.reverse() } else { order };
        }
        (a, b, c, d) = (b, r, d, s);
        reversed = !reversed;
    }
}

// For B~Binomial(105, 1/2), choose the greatest k with P(B<k)<=1/20.
// The one-based endpoints are k and n+1-k. All coefficients, intermediate
// products, and sums at this predeclared n fit u128. No normal approximation.
fn bounds() -> (usize, usize, u128, u128) {
    let total = 1_u128 << PAIRS;
    let (mut coefficient, mut cumulative, mut rank, mut tail) = (1_u128, 0_u128, 0, 0);
    for index in 0..=PAIRS / 2 {
        cumulative += coefficient;
        if cumulative * 20 > total { break; }
        rank = index + 1;
        tail = cumulative;
        coefficient = coefficient * (PAIRS - index) as u128 / (index + 1) as u128;
    }
    (rank, PAIRS + 1 - rank, total - tail, total)
}

type GroupKey = (String, String, Identity, String);

fn comparisons(raw: &[Value]) -> ToolResult<Vec<Value>> {
    let mut groups: BTreeMap<GroupKey, Vec<&Value>> = BTreeMap::new();
    for row in raw {
        groups.entry((text(row, "job_id")?.to_owned(), text(row, "variant")?.to_owned(), super::integrity::identity(row)?, text(row, "metric")?.to_owned())).or_default().push(row);
    }
    let (lower_rank, upper_rank, confidence, total) = bounds();
    let mut output = Vec::with_capacity(groups.len());
    for ((job, variant, identity, metric), rows) in groups {
        let mut residuals = Vec::with_capacity(rows.len());
        for row in &rows {
            let baseline = row["baseline"].as_u64().ok_or("baseline missing")?;
            let candidate = row["candidate"].as_u64().ok_or("candidate missing")?;
            residuals.push(Residual { numerator: i128::from(candidate) - i128::from(baseline) - i128::from(policy::active_allowance(&metric, baseline)?), denominator: baseline.max(1) });
        }
        residuals.sort_unstable_by(|a, b| a.compare(*b));
        let complete = residuals.len() == PAIRS;
        let (median, lower, upper, classification) = if complete {
            let lower = residuals[lower_rank - 1];
            let upper = residuals[upper_rank - 1];
            (residuals[PAIRS / 2].value(), lower.value(), upper.value(), if upper.numerator <= 0 { "passed" } else if lower.numerator > 0 { "regression" } else { "inconclusive" })
        } else { (Value::Null, Value::Null, Value::Null, "insufficient_evidence") };
        output.push(json!({"job_id": job, "role": rows[0]["role"], "variant": variant, "function_name": identity.0, "id": identity.1, "metric": metric, "samples": rows.len(), "classification": classification, "median": median, "lower": lower, "upper": upper, "lower_rank": lower_rank, "upper_rank": upper_rank, "one_sided_confidence": {"numerator": confidence, "denominator": total}, "raw_exceedances": rows.iter().filter(|row| row["current_passed"] == false).count()}));
    }
    Ok(output)
}

fn text<'a>(value: &'a Value, key: &str) -> ToolResult<&'a str> {
    value[key].as_str().ok_or_else(|| format!("missing {key}").into())
}

fn balanced(job: &Value) -> bool {
    let mut positions = [[0_usize; 5]; 5];
    let Some(samples) = job["samples"].as_array() else { return false; };
    for sample in samples {
        let Some(variant) = super::VARIANTS.iter().position(|v| sample["variant"] == *v) else { return false; };
        let Some(position) = sample["order"].as_u64().and_then(|v| usize::try_from(v).ok()).filter(|v| *v < 5) else { return false; };
        positions[variant][position] += 1;
    }
    positions.iter().flatten().all(|count| *count == PAIRS / 5)
}

fn build(jobs: &[Value], args: &ReportArgs) -> ToolResult<Value> {
    // Reuse the authoritative historical provenance and raw-policy diagnostics.
    // Historical conclusions are explicitly nested, not experiment verdicts.
    let historical = report::build_report(jobs)?;
    let raw = historical["comparisons"].as_array().ok_or("comparisons missing")?;
    let repeated = comparisons(raw)?;
    let expected_ids: Vec<_> = ["discovery", "validation"].into_iter().flat_map(|role| (1..=3).map(move |i| (format!("{role}-{i:02}"), role))).collect();
    let design = jobs.len() == 6 && expected_ids.iter().all(|(id, role)| jobs.iter().any(|j| j["job_id"] == *id && j["role"] == *role && balanced(j)));
    let expected = report::check_expected_coverage(jobs, args.expected_discovery_jobs, args.expected_validation_jobs, args.expected_pairs);
    let coverage = design && expected.is_ok() && args.expected_discovery_jobs.is_none_or(|n| n == 3) && args.expected_validation_jobs.is_none_or(|n| n == 3) && args.expected_pairs.is_none_or(|n| n == 105);
    let aa: Vec<_> = repeated.iter().filter(|r| r["variant"] == "b").collect();
    let failures: Vec<_> = aa.iter().filter(|r| r["classification"] != "passed").collect();
    let controls: Vec<_> = repeated.iter().filter(|r| r["role"] == "validation" && matches!(r["variant"].as_str(), Some("extra_work" | "extra_alloc")) && r["function_name"] == "inlay_hints_warm" && matches!(r["id"].as_str(), Some("small" | "medium" | "large")) && r["metric"] == "Ir").collect();
    let missed: Vec<_> = controls.iter().filter(|r| r["classification"] != "regression").collect();
    let repeatability = !aa.is_empty() && failures.is_empty();
    let sensitivity = controls.len() == 18 && missed.is_empty();
    Ok(json!({
        "format_version": 1, "method": METHOD, "mode": "report-only", "active_gates_changed": false, "evidence_verified": true,
        "coverage": {"planned_jobs": {"discovery": 3, "validation": 3}, "planned_pairs_per_job": 105, "predeclared_coverage_met": coverage, "observed_jobs": historical["coverage"]["observed_jobs"], "observed_pairs": historical["coverage"]["observed_pairs"], "balanced_execution_order": jobs.iter().all(balanced), "expectation_error": expected.err().map(|e| e.to_string())},
        "control_validation": {"passed": coverage && repeatability && sensitivity, "classification": if !coverage { "insufficient_evidence" } else if repeatability && sensitivity { "passed" } else { "failed" }, "coverage_sufficient": coverage, "repeatability_passed": repeatability, "sensitivity_passed": sensitivity, "repeatability_failures": failures, "sensitivity_failures": missed},
        "assumptions": "Conditional independent fresh-process samples within each job under randomized balanced execution order. No pooling across jobs, hosts, or builds. Reused a baselines make within-pair comparisons dependent. Bounds are one-sided marginal 95% bounds, not simultaneous or familywise claims. All upper bounds passing is an intersection-union criterion, conditional on sampling assumptions. This estimates typical median process change, not p95, tails, or editor latency.",
        "boundary_semantics": "Exact residual (candidate-baseline-active_allowance)/max(baseline,1). Ties are retained. Upper <= 0 passes; lower > 0 is regression; otherwise inconclusive. Median bounds are conservative for atoms/ties. Exactly 105 pairs; no adaptive stopping or retries.",
        "allowances": "Unchanged authoritative Ir floor 2%; cache ceil 3%, minimum 3. No learned allowances apply. Layout is diagnostic only, not assumed cost-equivalent.",
        "repeated_comparisons": repeated, "comparisons": raw,
        "jobs": historical["jobs"], "hosts": historical["hosts"],
        "historical_diagnostics": historical
    }))
}

fn markdown(report: &Value) -> ToolResult<String> {
    let mut output = format!("# Performance calibration: {METHOD} (report-only)\n\nActive gates changed: false. This experiment never grants performance admission.\n\n## Control validation\n\n{}\n\n## Coverage\n\n{}\n\n", report["control_validation"], report["coverage"]);
    if report["evidence_verified"] != true {
        writeln!(output, "Unverified evidence: {}", report["evidence_error"])?;
        return Ok(output);
    }
    writeln!(output, "{}\n\n{}\n\n{}\n\n## Per-job exact median residual bounds\n\n| Job | Variant | Workload | Event | n | Median | Lower | Upper | Classification |\n| --- | --- | --- | --- | ---: | --- | --- | --- | --- |", text(report, "assumptions")?, text(report, "boundary_semantics")?, text(report, "allowances")?)?;
    for row in report["repeated_comparisons"].as_array().ok_or("comparisons missing")? {
        let safe = |key| row[key].to_string().replace('|', "\\|").replace('\n', " ");
        writeln!(output, "| {} | {} | {} / {} | {} | {} | {} | {} | {} | {} |", safe("job_id"), safe("variant"), safe("function_name"), safe("id"), row["metric"], row["samples"], row["median"], row["lower"], row["upper"], row["classification"])?;
    }
    output.push_str("\nAll raw pairs, unchanged single-pair verdicts, exact confidence fractions/ranks, and historical proposal diagnostics are retained in JSON. Historical diagnostics do not determine this experiment's verdict; learned allowances never apply.\n");
    Ok(output)
}

pub(super) fn run(args: &ReportArgs) -> ToolResult<()> {
    let result = report::read_datasets(&args.input_root).and_then(|jobs| build(&jobs, args));
    let report = match result {
        Ok(report) => report,
        Err(error) => json!({"format_version": 1, "method": METHOD, "mode": "report-only", "active_gates_changed": false, "evidence_verified": false, "evidence_error": error.to_string(), "coverage": {"planned_jobs": {"discovery": 3, "validation": 3}, "planned_pairs_per_job": 105, "predeclared_coverage_met": false}, "control_validation": {"passed": false, "classification": "insufficient_evidence", "coverage_sufficient": false, "repeatability_passed": false, "sensitivity_passed": false}}),
    };
    write_json(&args.json_output, &report)?;
    if let Some(parent) = args.markdown_output.parent() { fs::create_dir_all(parent)?; }
    fs::write(&args.markdown_output, markdown(&report)?)?;
    require(report["control_validation"]["passed"] == true, "Calibration control validation failed: paired-median-105-v1 requires verified balanced 3/3/105 coverage, every A/A passed, and every held-out positive control classified regression. Reports retained.")
}
