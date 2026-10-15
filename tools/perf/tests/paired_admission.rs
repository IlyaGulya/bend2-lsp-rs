#[path = "support/paired.rs"]
mod paired;

use paired::TestResult;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn finalize(root: &Path, manifest: &Value, policy_exit: u8) -> TestResult<Output> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-perf"));
    command.args(["workflow", "finalize"])
        .env("GITHUB_ACTIONS", "true")
        .env("PERF_ROOT", root)
        .env("MODE", "callgrind")
        .env("MEASUREMENT_METHOD", "paired-median-105-v1")
        .env("PERF_RUNNER", "ubuntu-24.04")
        .env("SCENARIO", "analysis-hot-paths")
        .env("SELECTED_TARGET", "all")
        .env("GITHUB_JOB", "compare")
        .env("WORKFLOW_HEAD_SHA", "b".repeat(40))
        .env("STEPS_JSON", json!({
            "baseline": {"outcome": "success"},
            "candidate": {"outcome": if policy_exit == 0 { "success" } else { "failure" },
                "outputs": {"benchmark_exit_code": "0", "policy_exit_code": policy_exit.to_string()}}
        }).to_string());
    for (variable, field) in [
        ("REQUEST_ID", "request_id"),
        ("PERF_TARGET", "target"),
        ("BASE_SHA", "base_sha"),
        ("CANDIDATE_SHA", "candidate_sha"),
        ("WORKFLOW_SHA", "workflow_sha"),
        ("GITHUB_REPOSITORY", "repository"),
        ("GITHUB_RUN_ID", "run_id"),
        ("GITHUB_RUN_ATTEMPT", "run_attempt"),
    ] {
        command.env(
            variable,
            manifest[field]
                .as_str()
                .ok_or("missing fixture provenance")?,
        );
    }
    Ok(command.output()?)
}

fn admit(root: &Path, policy_exit: u8, attempt: &str, workflow: &str) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["admission", "measurement", "--root"])
        .arg(root)
        .arg("--base-sha")
        .arg("a".repeat(40))
        .arg("--candidate-sha")
        .arg("b".repeat(40))
        .arg("--head-sha")
        .arg("b".repeat(40))
        .arg("--run-attempt")
        .arg(attempt)
        .arg("--workflow-sha")
        .arg(workflow)
        .args([
            "--run-id",
            "12345",
            "--benchmark-exit-code",
            "0",
            "--measurement-method",
            "paired-median-105-v1",
        ])
        .arg("--policy-exit-code")
        .arg(policy_exit.to_string())
        .output()?)
}

#[test]
fn retained_uncertainty_cannot_be_classified_as_pass_or_confirmed_regression() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let manifest =
        paired::write_fixture(root, 1000, |pair| if pair % 2 == 1 { 1021 } else { 1019 })?;
    let report = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["paired", "report"])
        .arg(root)
        .arg("--json-output")
        .arg(root.join("callgrind-policy.json"))
        .arg("--markdown-output")
        .arg(root.join("callgrind-policy.md"))
        .output()?;
    assert_eq!(
        report.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let finalized = finalize(root, &manifest, 3)?;
    assert!(
        finalized.status.success(),
        "{}",
        String::from_utf8_lossy(&finalized.stderr)
    );
    let output = admit(root, 3, "1", &"c".repeat(40))?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["status"], "inconclusive");
    assert!(
        result["failures"]
            .as_array()
            .is_some_and(
                |failures| failures.iter().any(|failure| failure["function_name"]
                    == "inlay_hints_warm"
                    && failure["id"] == "small"
                    && failure["metric"] == "Ir"
                    && failure["classification"] == "inconclusive")
            )
    );
    Ok(())
}

#[test]
fn admission_requires_current_method_and_coherent_producer_outcomes() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    let manifest = paired::write_fixture(root, 1000, |_| 1000)?;
    let report = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["paired", "report"])
        .arg(root)
        .arg("--json-output")
        .arg(root.join("callgrind-policy.json"))
        .arg("--markdown-output")
        .arg(root.join("callgrind-policy.md"))
        .output()?;
    assert_eq!(
        report.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&report.stderr)
    );
    let finalized = finalize(root, &manifest, 0)?;
    assert!(
        finalized.status.success(),
        "{}",
        String::from_utf8_lossy(&finalized.stderr)
    );
    let passed = admit(root, 0, "1", &"c".repeat(40))?;
    assert!(
        passed.status.success(),
        "{}",
        String::from_utf8_lossy(&passed.stderr)
    );
    let result: Value = serde_json::from_slice(&passed.stdout)?;
    assert_eq!(result["status"], "passed");
    assert_eq!(result["run_attempt"], "1");
    assert_eq!(result["workflow_sha"], "c".repeat(40));
    for (attempt, workflow) in [("2", "c".repeat(40)), ("1", "d".repeat(40))] {
        let stale = admit(root, 0, attempt, &workflow)?;
        assert_eq!(
            stale.status.code(),
            Some(1),
            "An internally consistent bundle from another attempt or workflow must be rejected"
        );
    }
    let path = root.join("artifact-manifest.json");
    let original: Value = serde_json::from_slice(&fs::read(&path)?)?;
    for case in [
        "missing-method",
        "wrong-exit",
        "failed-baseline",
        "stale-head",
    ] {
        let mut changed = original.clone();
        match case {
            "missing-method" => {
                changed
                    .as_object_mut()
                    .ok_or("fixture manifest must be an object")?
                    .remove("measurement_method");
            }
            "wrong-exit" => changed["statuses"]["candidate"]["policy_exit_code"] = 1.into(),
            "failed-baseline" => changed["statuses"]["baseline"]["status"] = "failure".into(),
            "stale-head" => changed["gate_identity"]["head_sha"] = "c".repeat(40).into(),
            _ => return Err("unknown admission boundary".into()),
        }
        fs::write(&path, serde_json::to_vec_pretty(&changed)?)?;
        let rejected = admit(root, 0, "1", &"c".repeat(40))?;
        assert_eq!(
            rejected.status.code(),
            Some(1),
            "{case}: {}",
            String::from_utf8_lossy(&rejected.stderr)
        );
    }
    Ok(())
}
