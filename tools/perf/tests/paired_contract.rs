#[path = "support/paired.rs"]
mod paired;

use paired::{TestResult, write_fixture};
use serde_json::{Value, json};
use std::{fs, process::Command};

#[test]
fn paired_report_accepts_an_isolated_outlier_with_complete_105_sample_bounds() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    write_fixture(root, 1000, |pair| if pair == 1 { 10_000 } else { 1000 })?;
    let verdict_path = root.join("callgrind-policy.json");
    let markdown_path = root.join("callgrind-policy.md");
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["paired", "report"])
        .arg(root)
        .arg("--json-output")
        .arg(&verdict_path)
        .arg("--markdown-output")
        .arg(&markdown_path)
        .output()?;
    assert_eq!(output.status.code(), Some(0), "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let verdict: Value = serde_json::from_slice(&fs::read(verdict_path)?)?;
    assert_eq!(verdict["status"], "complete");
    assert_eq!(verdict["method"], "paired-median-105-v1");
    assert_eq!(verdict["workload_count"], 6);
    assert_eq!(verdict["passed_count"], 6);
    assert_eq!(verdict["failures"], json!([]));
    assert_eq!(verdict["new_workloads"], json!([]));
    let workloads = verdict["workloads"].as_array().ok_or("missing verdict workloads")?;
    assert_eq!(workloads.len(), 6);
    for (function, denominator, exceedances) in [
        ("inlay_hints_warm", 1000, 1),
        ("reference_location_order_warm", 2000, 0),
    ] {
        let workload = workloads.iter().find(|row| row["function_name"] == function && row["id"] == "small")
            .ok_or("missing benchmark's small workload")?;
        let comparison = &workload["metrics"]["Ir"];
        assert_eq!(comparison["classification"], "passed");
        assert_eq!(comparison["samples"], 105);
        assert!(comparison["lower"]["numerator"].as_i64().is_some_and(|value| value < 0));
        assert_eq!(comparison["lower"]["denominator"], denominator);
        assert!(comparison["upper"]["numerator"].as_i64().is_some_and(|value| value < 0));
        assert_eq!(comparison["raw_exceedances"], exceedances);
    }
    assert!(!fs::read(markdown_path)?.is_empty());
    Ok(())
}

#[test]
fn paired_report_preserves_zero_baseline_counts_without_accepting_missing_instructions() -> TestResult {
    for (baseline, expected_exit) in [(0, 0), (1000, 2)] {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        write_fixture(root, baseline, |_| 0)?;
        let verdict_path = root.join("callgrind-policy.json");
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args(["paired", "report"])
            .arg(root)
            .arg("--json-output")
            .arg(&verdict_path)
            .arg("--markdown-output")
            .arg(root.join("callgrind-policy.md"))
            .output()?;
        assert_eq!(output.status.code(), Some(expected_exit), "baseline={baseline}, stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        if baseline == 0 {
            let verdict: Value = serde_json::from_slice(&fs::read(verdict_path)?)?;
            assert_eq!(verdict["status"], "complete");
            let workload = verdict["workloads"].as_array().ok_or("missing workloads")?
                .iter().find(|row| row["function_name"] == "inlay_hints_warm" && row["id"] == "small")
                .ok_or("missing zero baseline workload")?;
            assert_eq!(workload["metrics"]["Ir"]["upper"], json!({"numerator": 0, "denominator": 1}));
            assert_eq!(workload["metrics"]["Ir"]["samples"], 105);
        }
    }
    Ok(())
}
