#[path = "support/paired.rs"]
mod paired;

use paired::TestResult;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

fn inconclusive_bundle(root: &Path) -> TestResult<(Value, Value)> {
    let mut manifest = paired::write_fixture(root, 1000, |pair| if pair <= 52 { 1000 } else { 1040 })?;
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["paired", "report"])
        .arg(root)
        .arg("--json-output")
        .arg(root.join("callgrind-policy.json"))
        .arg("--markdown-output")
        .arg(root.join("callgrind-policy.md"))
        .output()?;
    assert_eq!(
        output.status.code(),
        Some(3),
        "report producer: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let verdict: Value = serde_json::from_reader(fs::File::open(root.join("callgrind-policy.json"))?)?;
    assert_eq!(verdict["status"], "inconclusive");
    manifest["statuses"] = json!({
        "baseline":{"status":"success"},
        "callgrind-data":{"status":"success"},
        "candidate":{"status":"failure","benchmark_exit_code":0,"policy_exit_code":3}
    });
    paired::refresh_inventory(root, &mut manifest)?;
    Ok((manifest, verdict))
}

#[test]
fn repeated_inconclusive_bundle_remains_uncertain_without_corrupting_evidence() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let (_, verdict) = inconclusive_bundle(root)?;

    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .arg("dashboard")
        .arg(root)
        .arg("--target-only")
        .output()?;
    assert!(!output.status.success());
    let report: Value = serde_json::from_reader(fs::File::open(root.join("unified-report.json"))?)?;
    assert_eq!(report["status"], "inconclusive", "run errors: {}; target errors: {}",
        report["errors"], report["targets"][0]["errors"]);
    assert_eq!(report["errors"], json!([]));
    let target = &report["targets"][0];
    assert_eq!(target["status"], "inconclusive");
    assert_eq!(target["errors"], json!([]));
    assert_eq!(target["missing"], json!([]));
    assert_eq!(target["regressions"], json!([]));
    let document = target["documents"]
        .as_array()
        .and_then(|documents| documents.iter().find(|document| document["kind"] == "callgrind"))
        .ok_or("Missing retained gate document")?;
    assert_eq!(document["status"], "inconclusive");
    assert!(document["data"] == verdict, "Dashboard must retain the complete authoritative verdict");
    let metric = &document["data"]["workloads"][0]["metrics"]["Ir"];
    assert_eq!(metric["classification"], "inconclusive");
    assert_eq!(metric["samples"], 105);
    assert!(metric["lower"]["numerator"].as_i64().is_some_and(|value| value < 0));
    assert!(metric["upper"]["numerator"].as_i64().is_some_and(|value| value > 0));
    assert!(metric.get("baseline").is_none());
    assert!(metric.get("candidate").is_none());
    assert!(fs::metadata(root.join("index.html"))?.len() > 0);
    assert!(fs::metadata(root.join("summary.md"))?.len() > 0);
    Ok(())
}

#[test]
fn coherently_forged_repeated_pass_is_rejected_against_retained_raw_evidence() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let (mut manifest, mut verdict) = inconclusive_bundle(root)?;
    verdict["status"] = json!("complete");
    verdict["passed"] = json!(true);
    verdict["passed_count"] = verdict["workload_count"].clone();
    verdict["failures"] = json!([]);
    for workload in verdict["workloads"].as_array_mut().ok_or("Missing fixture workloads")? {
        for comparison in workload["metrics"].as_object_mut().ok_or("Missing fixture metrics")?.values_mut() {
            let bound = comparison["lower"].clone();
            assert!(bound["numerator"].as_i64().is_some_and(|value| value < 0));
            comparison["classification"] = json!("passed");
            comparison["median"] = bound.clone();
            comparison["lower"] = bound.clone();
            comparison["upper"] = bound;
            comparison["raw_exceedances"] = json!(0);
        }
    }
    fs::write(root.join("callgrind-policy.json"), serde_json::to_vec(&verdict)?)?;
    manifest["statuses"]["candidate"] = json!({
        "status":"success","benchmark_exit_code":0,"policy_exit_code":0
    });
    paired::refresh_inventory(root, &mut manifest)?;

    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .arg("dashboard")
        .arg(root)
        .arg("--target-only")
        .output()?;
    assert!(!output.status.success(), "Forged pass must not satisfy the dashboard gate");
    let report: Value = serde_json::from_reader(fs::File::open(root.join("unified-report.json"))?)?;
    assert_eq!(report["status"], "failed", "run errors: {}; target errors: {}",
        report["errors"], report["targets"][0]["errors"]);
    let target = &report["targets"][0];
    assert_eq!(target["status"], "failed");
    assert!(target["errors"].as_array().is_some_and(|errors| !errors.is_empty()));
    assert_eq!(target["regressions"], json!([]));
    let document = target["documents"]
        .as_array()
        .and_then(|documents| documents.iter().find(|document| document["kind"] == "callgrind"))
        .ok_or("Missing rejected gate document")?;
    assert_eq!(document["status"], "failed");
    Ok(())
}
