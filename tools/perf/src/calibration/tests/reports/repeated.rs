use super::*;

const FLAGS: &[&str] = &["--method", "paired-median-105-v1", "--require-control-validation", "--expected-discovery-jobs", "3", "--expected-validation-jobs", "3", "--expected-pairs", "105"];

fn jobs() -> ToolResult<Vec<Value>> {
    let mut jobs: Vec<_> = ["discovery", "validation"].into_iter().flat_map(|role| (1..=3).map(move |i| dataset(&format!("{role}-{i:02}"), role, 105))).collect();
    for job in &mut jobs {
        for sample in job["samples"].as_array_mut().ok_or("samples missing")? {
            let pair = sample["pair"].as_u64().ok_or("pair missing")?;
            let position = sample["order"].as_u64().ok_or("order missing")?;
            sample["order"] = json!((position + pair - 1) % 5);
        }
    }
    Ok(jobs)
}

fn output(fixture: &Fixture) -> ToolResult<Value> {
    Ok(serde_json::from_slice(&fs::read(fixture.directory.path().join("report.json"))?)?)
}

fn aa(report: &Value, job: &str, metric: &str) -> ToolResult<Value> {
    report["repeated_comparisons"].as_array().ok_or("repeated comparisons missing")?.iter().find(|r| r["job_id"] == job && r["variant"] == "b" && r["metric"] == metric && r["id"] == "small").cloned().ok_or_else(|| "A/A comparison missing".into())
}

#[test]
fn repeated_cli_accepts_exact_boundary_retains_raw_failures_and_fixed_bounds() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    let mut jobs = jobs()?;
    for job in &mut jobs { counts(job, "b", "Ir", 102, None)?; }
    counts(&mut jobs[0], "b", "Ir", u64::MAX, Some(1))?;
    fixture.load_jobs(&jobs)?;
    fixture.cli(FLAGS)?;
    let report = output(&fixture)?;
    assert_eq!(report["method"], "paired-median-105-v1");
    assert_eq!(report["active_gates_changed"], false);
    assert_eq!(report["coverage"]["planned_pairs_per_job"], 105);
    assert_eq!(report["coverage"]["planned_jobs"]["discovery"], 3);
    assert_eq!(report["control_validation"]["passed"], true);
    let row = aa(&report, "discovery-01", "Ir")?;
    assert_eq!(row["classification"], "passed");
    assert_eq!(row["samples"], 105);
    assert_eq!(row["lower_rank"], 44);
    assert_eq!(row["upper_rank"], 62);
    assert_eq!(row["upper"]["numerator"], 0);
    assert!(report["comparisons"].as_array().ok_or("raw rows missing")?.iter().any(|r| r["variant"] == "b" && r["current_passed"] == false));
    assert!(fixture.markdown()?.contains("paired-median-105-v1"));
    Ok(())
}

#[test]
fn repeated_cli_does_not_pool_jobs_or_accept_inconclusive_controls() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    let mut jobs = jobs()?;
    counts(&mut jobs[0], "b", "Ir", 103, None)?;
    fixture.load_jobs(&jobs)?;
    assert!(fixture.cli(FLAGS).is_err());
    assert_eq!(aa(&output(&fixture)?, "discovery-01", "Ir")?["classification"], "regression");
    counts(&mut jobs[0], "b", "Ir", 100, None)?;
    for pair in 1..=53 { counts(&mut jobs[3], "extra_alloc", "Ir", 102, Some(pair))?; }
    fixture.load_jobs(&jobs)?;
    assert!(fixture.cli(FLAGS).is_err());
    let report = output(&fixture)?;
    assert_eq!(report["control_validation"]["sensitivity_passed"], false);
    assert!(report["repeated_comparisons"].as_array().ok_or("rows missing")?.iter().any(|r| r["variant"] == "extra_alloc" && r["classification"] == "inconclusive"));
    Ok(())
}

#[test]
fn repeated_cli_handles_zero_u64_and_cache_boundary_exactly() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    let mut jobs = jobs()?;
    for job in &mut jobs {
        counts(job, "a", "I1mr", u64::MAX, None)?;
        counts(job, "b", "I1mr", 0, None)?;
        counts(job, "a", "ILmr", 0, None)?;
        counts(job, "b", "ILmr", 3, None)?;
        counts(job, "a", "Ir", 0, None)?;
        counts(job, "b", "Ir", 0, None)?;
    }
    fixture.load_jobs(&jobs)?;
    fixture.cli(FLAGS)?;
    let report = output(&fixture)?;
    assert_eq!(aa(&report, "discovery-01", "ILmr")?["upper"], json!({"numerator": 0, "denominator": 1}));
    assert_eq!(aa(&report, "discovery-01", "I1mr")?["classification"], "passed");
    counts(&mut jobs[0], "b", "ILmr", 4, None)?;
    fixture.load_jobs(&jobs)?;
    assert!(fixture.cli(FLAGS).is_err());
    assert_eq!(aa(&output(&fixture)?, "discovery-01", "ILmr")?["classification"], "regression");
    Ok(())
}

#[test]
fn repeated_cli_writes_failure_reports_for_insufficient_or_unverified_evidence() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    let mut jobs = jobs()?;
    jobs[0]["samples"].as_array_mut().ok_or("samples missing")?.retain(|s| s["pair"] != 105);
    fixture.load_jobs(&jobs)?;
    assert!(fixture.cli(FLAGS).is_err());
    let report = output(&fixture)?;
    assert_eq!(report["control_validation"]["passed"], false);
    assert_eq!(aa(&report, "discovery-01", "Ir")?["classification"], "insufficient_evidence");
    assert!(!fixture.markdown()?.is_empty());
    fs::write(fixture.input.join("0/nested/raw/1-b.stdout"), "stale")?;
    assert!(fixture.cli(FLAGS).is_err());
    let report = output(&fixture)?;
    assert_eq!(report["evidence_verified"], false);
    assert_eq!(report["control_validation"]["passed"], false);
    assert!(!report["evidence_error"].as_str().ok_or("error missing")?.is_empty());
    Ok(())
}

#[test]
fn repeated_cli_binomial_boundary_requires_62_of_105_and_preserves_ties() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    let mut jobs = jobs()?;
    counts(&mut jobs[0], "b", "Ir", 103, None)?;
    for pair in 1..=61 {
        counts(&mut jobs[0], "b", "Ir", 102, Some(pair))?;
    }
    fixture.load_jobs(&jobs)?;
    assert!(fixture.cli(FLAGS).is_err());
    assert_eq!(aa(&output(&fixture)?, "discovery-01", "Ir")?["classification"], "inconclusive");
    counts(&mut jobs[0], "b", "Ir", 102, Some(62))?;
    fixture.load_jobs(&jobs)?;
    fixture.cli(FLAGS)?;
    let row = aa(&output(&fixture)?, "discovery-01", "Ir")?;
    assert_eq!(row["classification"], "passed");
    assert_eq!(row["upper"]["numerator"], 0);
    Ok(())
}

#[test]
fn repeated_cli_rejects_missing_roles_duplicate_jobs_and_unbalanced_positions() -> ToolResult<()> {
    for kind in ["missing", "duplicate", "unbalanced"] {
        let fixture = Fixture::new()?;
        let mut jobs = jobs()?;
        match kind {
            "missing" => jobs.retain(|j| j["role"] == "discovery"),
            "duplicate" => jobs[1] = jobs[0].clone(),
            _ => {
                for sample in jobs[0]["samples"].as_array_mut().ok_or("samples missing")? {
                    let variant = sample["variant"].as_str().ok_or("variant missing")?;
                    sample["order"] = json!(VARIANTS.iter().position(|v| *v == variant).ok_or("variant unknown")?);
                }
            }
        }
        for (index, job) in jobs.iter().enumerate() {
            write_job(&fixture.input.join(index.to_string()), job)?;
        }
        assert!(fixture.cli(FLAGS).is_err());
        let report = output(&fixture)?;
        assert_eq!(report["control_validation"]["passed"], false);
        assert!(!fixture.markdown()?.is_empty());
    }
    Ok(())
}
