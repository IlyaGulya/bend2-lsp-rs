use serde_json::json;
use std::{fs, path::Path, process::Command};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn git(root: &Path, arguments: &[&str]) -> TestResult<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=Performance fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(arguments)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "fixture git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn source(root: &Path) -> TestResult<String> {
    fs::create_dir(root)?;
    fs::write(root.join("input.bend"), "def main() -> U32:\n  0\n")?;
    git(root, &["init", "--quiet"])?;
    git(root, &["add", "--", "input.bend"])?;
    git(root, &["commit", "--quiet", "--message", "Fixture source"])?;
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn paired_collection_rejects_wrong_source_revision_before_compilation() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let baseline = temporary.path().join("baseline");
    let candidate = temporary.path().join("candidate");
    let base_sha = source(&baseline)?;
    let actual_candidate = source(&candidate)?;
    let output = temporary.path().join("evidence");
    fs::create_dir(&output)?;
    let manifest = json!({
        "schema_version": 1, "mode": "callgrind", "measurement_method": "paired-median-105-v1",
        "target": "x86_64-unknown-linux-gnu", "base_sha": base_sha, "candidate_sha": "0".repeat(40),
        "workflow_sha": "c".repeat(40), "repository": "owner/repository", "request_id": "collection-fixture",
        "run_id": "12345", "run_attempt": "1",
        "gate_identity": {"workflow": "performance", "job": "compare", "step": "Compare pull request with main", "head_sha": "0".repeat(40)},
        "files": [], "expected": []
    });
    fs::write(
        output.join("artifact-manifest.json"),
        serde_json::to_vec(&manifest)?,
    )?;
    let result = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["paired", "collect", "--baseline-source"])
        .arg(&baseline)
        .arg("--candidate-source")
        .arg(&candidate)
        .arg("--work-dir")
        .arg(temporary.path().join("work"))
        .arg("--output-dir")
        .arg(&output)
        .env("GITHUB_ACTIONS", "true")
        .output()?;
    assert_eq!(
        result.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let failure: serde_json::Value =
        serde_json::from_reader(fs::File::open(output.join("paired-failure.json"))?)?;
    assert_eq!(
        failure["reason"],
        json!({
            "kind": "source_revision_mismatch", "variant": "b",
            "expected": "0".repeat(40), "actual": actual_candidate
        })
    );
    assert!(
        !output.join("paired-data.json").exists(),
        "unverified source must never produce completed evidence"
    );
    Ok(())
}
