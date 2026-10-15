#[path = "support/paired.rs"]
mod paired;

use paired::TestResult;
use serde_json::{Value, json};
use std::process::Command;

#[test]
fn retained_uncertainty_cannot_be_classified_as_pass_or_confirmed_regression() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let mut manifest = paired::write_fixture(root, 1000, |pair| if pair % 2 == 1 { 1021 } else { 1019 })?;
    let report = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["paired", "report"]).arg(root)
        .arg("--json-output").arg(root.join("callgrind-policy.json"))
        .arg("--markdown-output").arg(root.join("callgrind-policy.md"))
        .output()?;
    assert_eq!(report.status.code(), Some(3), "{}", String::from_utf8_lossy(&report.stderr));
    manifest["statuses"] = json!({
        "baseline": {"status": "success"}, "callgrind-data": {"status": "success"},
        "candidate": {"status": "failure", "benchmark_exit_code": 0, "policy_exit_code": 3}
    });
    paired::refresh_inventory(root, &mut manifest)?;
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["admission", "measurement", "--root"]).arg(root)
        .arg("--base-sha").arg("a".repeat(40))
        .arg("--candidate-sha").arg("b".repeat(40))
        .arg("--head-sha").arg("b".repeat(40))
        .args(["--run-id", "12345", "--benchmark-exit-code", "0", "--policy-exit-code", "3",
            "--measurement-method", "paired-median-105-v1"])
        .output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["status"], "inconclusive");
    assert!(result["failures"].as_array().is_some_and(|failures| failures.iter().any(|failure|
        failure["function_name"] == "inlay_hints_warm" && failure["id"] == "small"
            && failure["metric"] == "Ir" && failure["classification"] == "inconclusive")));
    Ok(())
}
