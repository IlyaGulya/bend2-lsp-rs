use super::{dataset, write_job};
use crate::{
    ToolResult,
    calibration::{VARIANTS, report},
    common::write_json,
};
use serde_json::{Value, json};
use std::{fs, path::Path};

fn dataset_v2(job_id: &str, role: &str) -> ToolResult<Value> {
    let mut job = dataset(job_id, role, 1);
    job["format_version"] = json!(2);
    job["harness_sha256"] =
        json!({"analysis": "e".repeat(64), "reference_locations": "d".repeat(64)});
    let analysis: Vec<_> = job["samples"][0]["workloads"]
        .as_array()
        .ok_or("workloads missing")?
        .iter()
        .map(|row| json!({"function_name": row["function_name"], "id": row["id"]}))
        .collect();
    let references: Vec<_> = ["small", "medium", "large"]
        .into_iter()
        .map(|id| json!({"function_name": "reference_location_order_warm", "id": id}))
        .collect();
    for variant in VARIANTS {
        let mut analysis_manifest = job["variants"][variant].clone();
        let source = analysis_manifest
            .as_object_mut()
            .ok_or("manifest missing")?
            .remove("source_sha256")
            .ok_or("source missing")?;
        analysis_manifest["workloads"] = json!(analysis);
        job["variants"][variant] = json!({"source_sha256": source, "benchmarks": {
            "analysis": analysis_manifest,
            "reference_locations": {"executable": format!("/isolated/{variant}/reference_locations"), "binary_sha256": "c".repeat(64), "workloads": references}
        }});
    }
    for sample in job["samples"].as_array_mut().ok_or("samples missing")? {
        let variant = sample["variant"]
            .as_str()
            .ok_or("variant missing")?
            .to_owned();
        let pair = sample["pair"].as_u64().ok_or("pair missing")?;
        let object = sample.as_object_mut().ok_or("sample missing")?;
        object.remove("raw_stdout");
        object.remove("raw_stderr");
        sample["benchmarks"] = json!({});
        for benchmark in ["analysis", "reference_locations"] {
            sample["benchmarks"][benchmark] = json!({
                "raw_stdout": format!("raw/{pair}-{variant}/{benchmark}/stdout"),
                "raw_stderr": format!("raw/{pair}-{variant}/{benchmark}/stderr"),
                "verified_callgrind_profiles": if benchmark == "analysis" { 4 } else { 3 }
            });
        }
        for reference in &references {
            sample["workloads"]
                .as_array_mut()
                .ok_or("workloads missing")?
                .push(json!({
                    "function_name": reference["function_name"], "id": reference["id"],
                    "counts": {"Ir": 200, "I1mr": 20, "ILmr": 10}
                }));
        }
    }
    Ok(job)
}

fn write_v2(root: &Path, job: &Value) -> ToolResult<()> {
    // Reuse the established real JSONL summary fixture writer, once per executable.
    for benchmark in ["analysis", "reference_locations"] {
        let mut projected = job.clone();
        for variant in VARIANTS {
            projected["variants"][variant] =
                job["variants"][variant]["benchmarks"][benchmark].clone();
        }
        for sample in projected["samples"]
            .as_array_mut()
            .ok_or("samples missing")?
        {
            sample["raw_stdout"] = sample["benchmarks"][benchmark]["raw_stdout"].clone();
            sample["raw_stderr"] = sample["benchmarks"][benchmark]["raw_stderr"].clone();
            sample["workloads"]
                .as_array_mut()
                .ok_or("workloads missing")?
                .retain(|row| {
                    (row["function_name"] == "reference_location_order_warm")
                        == (benchmark == "reference_locations")
                });
        }
        write_job(root, &projected)?;
    }
    write_json(&root.join("data.json"), job)
}

#[test]
fn v2_retained_two_benchmark_evidence_is_accepted_and_scope_is_explicit() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    for (id, role) in [("training", "discovery"), ("holdout", "validation")] {
        write_v2(&directory.path().join(id), &dataset_v2(id, role)?)?;
    }
    let jobs = report::read_datasets(directory.path())?;
    let report = report::build_report(&jobs)?;
    assert_eq!(
        report["benchmark_scope"],
        json!({"evidence_format_version": 2,
        "benchmarks": {"analysis": 4, "reference_locations": 3}, "workloads": 7})
    );
    assert_eq!(
        report["comparisons"]
            .as_array()
            .ok_or("comparisons missing")?
            .len(),
        168
    );
    assert_eq!(report["active_gates_changed"], false);
    Ok(())
}

#[test]
fn v2_evidence_rejects_missing_misattributed_and_forged_measurements() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let job = dataset_v2("training", "discovery")?;
    write_v2(directory.path(), &job)?;
    report::validate_job(&job, directory.path())?;
    let reference = &job["samples"][0]["benchmarks"]["reference_locations"];
    let stdout = directory
        .path()
        .join(reference["raw_stdout"].as_str().ok_or("stdout missing")?);
    let original = fs::read_to_string(&stdout)?;
    let first = original.lines().next().ok_or("summary missing")?;
    for text in [
        String::new(),
        format!("{original}{first}\n"),
        original.replace("/isolated/a/reference_locations", "/isolated/a/analysis"),
        original.replace("reference_location_order_warm", "inlay_hints_warm"),
        original.replace("\"Int\":200", "\"Int\":201"),
        original.replace(
            &crate::calibration::baseline_name("training", 1, "a"),
            "stale",
        ),
    ] {
        fs::write(&stdout, text)?;
        assert!(report::validate_job(&job, directory.path()).is_err());
    }
    fs::write(&stdout, original)?;
    for field in ["raw_stdout", "raw_stderr"] {
        let mut changed = job.clone();
        changed["samples"][0]["benchmarks"]["reference_locations"][field] = json!("../escape");
        assert!(report::validate_job(&changed, directory.path()).is_err());
    }
    let mut forged = job.clone();
    forged["samples"][0]["workloads"][4]["counts"]["Ir"] = json!(201);
    assert!(report::validate_job(&forged, directory.path()).is_err());
    let mut aliased = job.clone();
    aliased["samples"][0]["benchmarks"]["reference_locations"]["raw_stderr"] =
        job["samples"][0]["benchmarks"]["analysis"]["raw_stderr"].clone();
    assert!(report::validate_job(&aliased, directory.path()).is_err());
    fs::remove_file(
        directory
            .path()
            .join(reference["raw_stderr"].as_str().ok_or("stderr missing")?),
    )?;
    assert!(report::validate_job(&job, directory.path()).is_err());
    Ok(())
}

#[test]
fn v2_manifests_bind_complete_disjoint_identity_ownership() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let job = dataset_v2("training", "discovery")?;
    write_v2(directory.path(), &job)?;
    report::validate_job(&job, directory.path())?;
    let mut mutations = Vec::new();
    let mut missing = job.clone();
    missing["variants"]["b"]["benchmarks"]
        .as_object_mut()
        .ok_or("benchmarks missing")?
        .remove("reference_locations");
    mutations.push(missing);
    let mut missing_sample = job.clone();
    missing_sample["samples"][0]["benchmarks"]
        .as_object_mut()
        .ok_or("benchmarks missing")?
        .remove("reference_locations");
    mutations.push(missing_sample);
    let mut duplicate = job.clone();
    let stolen = duplicate["variants"]["a"]["benchmarks"]["analysis"]["workloads"][0].clone();
    duplicate["variants"]["a"]["benchmarks"]["reference_locations"]["workloads"]
        .as_array_mut()
        .ok_or("workloads missing")?
        .push(stolen);
    mutations.push(duplicate);
    let mut omitted = job.clone();
    omitted["variants"]["b"]["benchmarks"]["reference_locations"]["workloads"]
        .as_array_mut()
        .ok_or("workloads missing")?
        .pop();
    mutations.push(omitted);
    let mut wrong_harness = job.clone();
    wrong_harness["harness_sha256"] = json!({"analysis": "e".repeat(64)});
    mutations.push(wrong_harness);
    let mut wrong_probe = job.clone();
    wrong_probe["variants"]["b"]["benchmarks"]["reference_locations"]["layout_probe"] =
        job["variants"]["b"]["benchmarks"]["analysis"]["layout_probe"].clone();
    mutations.push(wrong_probe);
    let mut duplicate_identity = job.clone();
    let row = duplicate_identity["variants"]["b"]["benchmarks"]["reference_locations"]["workloads"]
        [0]
    .clone();
    duplicate_identity["variants"]["b"]["benchmarks"]["reference_locations"]["workloads"]
        .as_array_mut()
        .ok_or("workloads missing")?
        .push(row);
    mutations.push(duplicate_identity);
    let mut reassigned = job.clone();
    let row = reassigned["variants"]["b"]["benchmarks"]["analysis"]["workloads"]
        .as_array_mut()
        .ok_or("workloads missing")?
        .pop()
        .ok_or("workload missing")?;
    reassigned["variants"]["b"]["benchmarks"]["reference_locations"]["workloads"]
        .as_array_mut()
        .ok_or("workloads missing")?
        .push(row);
    mutations.push(reassigned);
    let mut wrong_binary = job.clone();
    wrong_binary["variants"]["b"]["benchmarks"]["reference_locations"]["executable"] =
        job["variants"]["b"]["benchmarks"]["analysis"]["executable"].clone();
    mutations.push(wrong_binary);
    let mut collected_probe = job.clone();
    collected_probe["variants"]["a"]["benchmarks"]["analysis"]["layout_probe_collected"] =
        json!(true);
    mutations.push(collected_probe);
    let mut missing_profiles = job.clone();
    missing_profiles["samples"][0]["benchmarks"]["reference_locations"]["verified_callgrind_profiles"] =
        json!(0);
    mutations.push(missing_profiles);
    for changed in mutations {
        assert!(report::validate_job(&changed, directory.path()).is_err());
    }
    Ok(())
}

#[test]
fn historical_scope_stays_analysis_only_and_mixed_experiments_fail() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let old = dataset("training", "discovery", 1);
    let old_validation = dataset("holdout", "validation", 1);
    write_job(&directory.path().join("old"), &old)?;
    write_job(&directory.path().join("held"), &old_validation)?;
    let report = report::build_report(&report::read_datasets(directory.path())?)?;
    assert_eq!(
        report["benchmark_scope"],
        json!({"evidence_format_version": 1,
        "benchmarks": {"analysis": 4}, "workloads": 4})
    );
    let fresh = dataset_v2("holdout", "validation")?;
    write_v2(&directory.path().join("held"), &fresh)?;
    assert!(report::read_datasets(directory.path()).is_err());
    let mut different = dataset_v2("training", "discovery")?;
    different["harness_sha256"]["reference_locations"] = json!("b".repeat(64));
    write_v2(&directory.path().join("old"), &different)?;
    assert!(report::read_datasets(directory.path()).is_err());
    Ok(())
}

#[test]
fn repeated_v2_reports_both_benchmarks_without_changing_fixed_control_math() -> ToolResult<()> {
    let fixture = super::Fixture::new()?;
    for role in ["discovery", "validation"] {
        for index in 1..=3 {
            let id = format!("{role}-{index:02}");
            let mut job = dataset_v2(&id, role)?;
            let original = job["samples"].as_array().ok_or("samples missing")?.clone();
            let mut samples = Vec::new();
            for pair in 1..=105 {
                for (position, sample) in original.iter().enumerate() {
                    let mut sample = sample.clone();
                    sample["pair"] = json!(pair);
                    sample["order"] = json!((position + pair - 1) % 5);
                    let variant = sample["variant"]
                        .as_str()
                        .ok_or("variant missing")?
                        .to_owned();
                    for benchmark in ["analysis", "reference_locations"] {
                        sample["benchmarks"][benchmark]["raw_stdout"] =
                            json!(format!("raw/{pair}-{variant}/{benchmark}/stdout"));
                        sample["benchmarks"][benchmark]["raw_stderr"] =
                            json!(format!("raw/{pair}-{variant}/{benchmark}/stderr"));
                    }
                    samples.push(sample);
                }
            }
            job["samples"] = json!(samples);
            write_v2(&fixture.input.join(id), &job)?;
        }
    }
    fixture.cli(&[
        "--method",
        "paired-median-105-v1",
        "--require-control-validation",
        "--require-all-benchmarks",
        "--expected-workloads",
        "7",
    ])?;
    let report: Value =
        serde_json::from_slice(&fs::read(fixture.directory.path().join("report.json"))?)?;
    assert_eq!(
        report["benchmark_scope"]["benchmarks"],
        json!({"analysis": 4, "reference_locations": 3})
    );
    assert_eq!(report["benchmark_scope"]["workloads"], 7);
    assert_eq!(report["control_validation"]["passed"], true);
    let rows = report["repeated_comparisons"]
        .as_array()
        .ok_or("comparisons missing")?;
    assert_eq!(rows.len(), 504);
    assert!(
        rows.iter()
            .all(|row| row["samples"] == 105 && row["lower_rank"] == 44 && row["upper_rank"] == 62)
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row["role"] == "validation"
                && matches!(row["variant"].as_str(), Some("extra_work" | "extra_alloc"))
                && row["function_name"] == "inlay_hints_warm"
                && row["metric"] == "Ir"
                && row["classification"] == "regression")
            .count(),
        18
    );
    assert!(
        rows.iter()
            .filter(|row| row["function_name"] == "reference_location_order_warm")
            .all(|row| row["classification"] == "passed")
    );
    Ok(())
}

#[test]
fn strict_cli_scope_rejects_historical_evidence_but_retains_verified_report() -> ToolResult<()> {
    let fixture = super::Fixture::new()?;
    fixture.load()?;
    assert!(
        fixture
            .cli(&[
                "--method",
                "paired-median-105-v1",
                "--require-all-benchmarks"
            ])
            .is_err()
    );
    let report: Value =
        serde_json::from_slice(&fs::read(fixture.directory.path().join("report.json"))?)?;
    assert_eq!(report["evidence_verified"], true);
    assert_eq!(report["control_validation"]["passed"], false);
    assert!(
        report["coverage"]["expectation_error"]
            .as_str()
            .is_some_and(|error| error.contains("benchmark"))
    );
    assert!(fixture.markdown()?.contains("analysis"));
    Ok(())
}

#[test]
fn strict_cli_workload_count_rejects_omission_but_retains_verified_report() -> ToolResult<()> {
    let fixture = super::Fixture::new()?;
    for (id, role) in [("training", "discovery"), ("holdout", "validation")] {
        write_v2(&fixture.input.join(id), &dataset_v2(id, role)?)?;
    }
    assert!(
        fixture
            .cli(&[
                "--method",
                "paired-median-105-v1",
                "--expected-workloads",
                "8"
            ])
            .is_err()
    );
    let report: Value =
        serde_json::from_slice(&fs::read(fixture.directory.path().join("report.json"))?)?;
    assert_eq!(report["evidence_verified"], true);
    assert_eq!(report["benchmark_scope"]["workloads"], 7);
    assert_eq!(report["control_validation"]["passed"], false);
    assert!(
        report["coverage"]["expectation_error"]
            .as_str()
            .is_some_and(|error| error.contains("expected 8") && error.contains("found 7"))
    );
    assert!(fixture.markdown()?.contains("reference_locations"));
    Ok(())
}
