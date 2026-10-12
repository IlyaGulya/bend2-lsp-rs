use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const METRICS: [&str; 3] = ["Ir", "I1mr", "ILmr"];

fn summary(index: usize, candidate: [u64; 3], baseline: [u64; 3]) -> Value {
    let mut callgrind = serde_json::Map::new();
    for (position, metric) in METRICS.iter().enumerate() {
        callgrind.insert((*metric).to_owned(), json!({"metrics": {"Both": [{"Int": candidate[position]}, {"Int": baseline[position]}]}}));
    }
    json!({
        "function_name": format!("analysis::workload_{:02}", index / 3),
        "id": format!("case_{index:03}"),
        "baselines": [null, "selected"],
        "profiles": [{"summaries": {"parts": [{"metrics_summary": {"Callgrind": callgrind}}]}}]
    })
}

fn metrics(data: &mut Value) -> TestResult<&mut serde_json::Map<String, Value>> {
    data.pointer_mut("/profiles/0/summaries/parts/0/metrics_summary/Callgrind")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| "missing fixture metrics".into())
}

fn left_only(mut data: Value) -> TestResult<Value> {
    for metric in METRICS {
        let values = &mut metrics(&mut data)?[metric]["metrics"];
        let current = values["Both"][0].clone();
        *values = json!({"Left": current});
    }
    Ok(data)
}

fn write_summary(root: &Path, filename: &str, data: &Value) -> TestResult<PathBuf> {
    let directory = root.join(filename);
    fs::create_dir_all(&directory)?;
    let path = directory.join("summary.json");
    fs::write(&path, serde_json::to_vec(data)?)?;
    Ok(path)
}

fn cli(root: &Path, args: &[String]) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .arg("policy")
        .arg(root)
        .args(args)
        .output()?)
}

fn text(bytes: &[u8]) -> TestResult<&str> {
    Ok(std::str::from_utf8(bytes)?)
}

fn assert_status(output: &Output, status: i32) -> TestResult {
    assert_eq!(
        output.status.code(),
        Some(status),
        "stdout={} stderr={}",
        text(&output.stdout)?,
        text(&output.stderr)?
    );
    Ok(())
}

fn baseline(root: &Path, count: usize) -> TestResult<PathBuf> {
    let baseline_root = root.join("baseline-summaries");
    for index in 0..count {
        let mut data = left_only(summary(index, [100, 2, 2], [100, 2, 2]))?;
        data["baselines"] = json!([null, "main"]);
        write_summary(&baseline_root, &format!("baseline-{index:03}"), &data)?;
    }
    let manifest = root.join("baseline-manifest.json");
    let output = cli(
        &baseline_root,
        &[format!("--write-baseline-manifest={}", manifest.display())],
    )?;
    assert_status(&output, 0)?;
    assert_eq!(
        text(&output.stdout)?,
        format!("Wrote baseline manifest with {count} unique benchmarks.\n")
    );
    let data: Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    assert_eq!(data["format_version"], json!(1));
    assert_eq!(
        data["benchmarks"]
            .as_array()
            .ok_or("benchmarks are not an array")?
            .len(),
        count
    );
    Ok(manifest)
}

fn candidates(
    root: &Path,
    candidate_count: usize,
    baseline_count: usize,
    regression: Option<usize>,
) -> TestResult {
    for index in 0..candidate_count {
        let current = if regression == Some(index) {
            [103, 2, 2]
        } else {
            [100, 2, 2]
        };
        let mut data = summary(index, current, [100, 2, 2]);
        if index >= baseline_count {
            data = left_only(data)?;
        }
        write_summary(root, &format!("candidate-{index:03}"), &data)?;
    }
    Ok(())
}

fn compare(root: &Path, manifest: &Path) -> TestResult<Output> {
    cli(
        root,
        &[
            "--baseline-name=selected".to_owned(),
            format!("--baseline-manifest={}", manifest.display()),
        ],
    )
}

#[test]
fn baseline_and_candidate_workload_counts_are_authoritative() -> TestResult {
    for (baseline_count, candidate_count, new_count, last_new) in [
        (25, 25, 0, None),
        (25, 36, 11, Some("analysis::workload_11 [case_035]")),
        (36, 36, 0, None),
        (36, 37, 1, Some("analysis::workload_12 [case_036]")),
    ] {
        let temporary = TempDir::new()?;
        let manifest = baseline(temporary.path(), baseline_count)?;
        let candidate_root = temporary.path().join("candidate");
        candidates(&candidate_root, candidate_count, baseline_count, None)?;
        let output = compare(&candidate_root, &manifest)?;
        assert_status(&output, 0)?;
        let stdout = text(&output.stdout)?;
        assert!(stdout.contains(&format!(
            "{baseline_count}/{baseline_count} baseline workloads passed"
        )));
        assert!(stdout.contains(&format!("{new_count} new candidate workloads")));
        assert_eq!(
            stdout
                .lines()
                .filter(|line| line.starts_with("NEW "))
                .count(),
            new_count
        );
        if let Some(last) = last_new {
            assert!(stdout.contains(last));
        }
    }
    Ok(())
}

#[test]
fn missing_baseline_id_fails_with_canonical_id() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 36)?;
    let root = temporary.path().join("candidate");
    candidates(&root, 35, 36, None)?;
    let output = compare(&root, &manifest)?;
    assert_status(&output, 2)?;
    assert!(
        text(&output.stderr)?
            .contains("missing baseline benchmark ID analysis::workload_11 [case_035]")
    );
    Ok(())
}

#[test]
fn duplicate_baseline_summary_id_is_a_hard_error() -> TestResult {
    let temporary = TempDir::new()?;
    let root = temporary.path().join("baseline");
    let data = left_only(summary(0, [100, 2, 2], [100, 2, 2]))?;
    write_summary(&root, "first", &data)?;
    write_summary(&root, "duplicate", &data)?;
    let output = cli(
        &root,
        &[format!(
            "--write-baseline-manifest={}",
            temporary.path().join("manifest.json").display()
        )],
    )?;
    assert_status(&output, 2)?;
    assert!(text(&output.stderr)?.contains("duplicate baseline benchmark ID"));
    Ok(())
}

#[test]
fn duplicate_candidate_id_is_a_hard_error() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 1)?;
    let root = temporary.path().join("candidate");
    let data = summary(0, [100, 2, 2], [100, 2, 2]);
    write_summary(&root, "first", &data)?;
    write_summary(&root, "duplicate", &data)?;
    let output = compare(&root, &manifest)?;
    assert_status(&output, 2)?;
    assert!(text(&output.stderr)?.contains("duplicate candidate benchmark ID"));
    Ok(())
}

#[test]
fn candidate_results_for_a_different_baseline_are_ignored() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 1)?;
    let root = temporary.path().join("candidate");
    write_summary(&root, "selected", &summary(0, [100, 2, 2], [100, 2, 2]))?;
    let mut other = summary(0, [1000, 1000, 1000], [100, 2, 2]);
    other["baselines"] = json!([null, "other"]);
    write_summary(&root, "other", &other)?;
    let output = compare(&root, &manifest)?;
    assert_status(&output, 0)?;
    assert!(text(&output.stdout)?.contains("1/1 baseline workloads passed"));
    Ok(())
}

#[test]
fn each_missing_required_metric_is_a_hard_error() -> TestResult {
    for metric in METRICS {
        let temporary = TempDir::new()?;
        let manifest = baseline(temporary.path(), 1)?;
        let root = temporary.path().join("candidate");
        let mut data = summary(0, [100, 2, 2], [100, 2, 2]);
        metrics(&mut data)?.remove(metric);
        write_summary(&root, "missing", &data)?;
        let output = compare(&root, &manifest)?;
        assert_status(&output, 2)?;
        assert!(
            text(&output.stderr)?.contains(&format!("missing required Callgrind metric {metric}"))
        );
    }
    Ok(())
}

#[test]
fn missing_paired_event_is_a_hard_error() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 1)?;
    let root = temporary.path().join("candidate");
    let mut data = summary(0, [100, 2, 2], [100, 2, 2]);
    data.pointer_mut("/profiles/0/summaries/parts/0/metrics_summary/Callgrind/I1mr/metrics")
        .and_then(Value::as_object_mut)
        .ok_or("missing fixture pair")?
        .remove("Both");
    write_summary(&root, "missing-pair", &data)?;
    let output = compare(&root, &manifest)?;
    assert_status(&output, 2)?;
    assert!(text(&output.stderr)?.contains("missing paired counts for I1mr"));
    Ok(())
}

#[test]
fn malformed_paired_metrics_are_a_hard_error() -> TestResult {
    for pair in [
        json!([{"Int": 100}]),
        json!([{"Int": -1}, {"Int": 100}]),
        json!([{"Int": true}, {"Int": 100}]),
        json!([{"Int": 100.0}, {"Int": 100}]),
        json!([100, 100]),
        json!({}),
    ] {
        let temporary = TempDir::new()?;
        let manifest = baseline(temporary.path(), 1)?;
        let root = temporary.path().join("candidate");
        let mut data = summary(0, [100, 2, 2], [100, 2, 2]);
        metrics(&mut data)?["Ir"]["metrics"]["Both"] = pair;
        write_summary(&root, "malformed", &data)?;
        let output = compare(&root, &manifest)?;
        assert_status(&output, 2)?;
        assert!(text(&output.stderr)?.contains("invalid paired integer counts for Ir"));
    }
    Ok(())
}

#[test]
fn regression_in_any_baseline_workload_fails() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 36)?;
    let root = temporary.path().join("candidate");
    candidates(&root, 36, 36, Some(35))?;
    let output = compare(&root, &manifest)?;
    assert_status(&output, 1)?;
    assert!(text(&output.stdout)?.contains("FAIL analysis::workload_11 [case_035]: Ir 100→103"));
    assert!(text(&output.stdout)?.contains("35/36 baseline workloads passed"));
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn zero_baseline_three_cache_events_pass_and_four_fail_on_summaries() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 1)?;
    let root = temporary.path().join("candidate");
    for (events, status) in [(3, 0), (4, 1)] {
        write_summary(
            &root,
            "zero",
            &summary(0, [100, events, events], [100, 0, 0]),
        )?;
        let output = compare(&root, &manifest)?;
        assert_status(&output, status)?;
        if events == 4 {
            assert!(
                text(&output.stdout)?
                    .contains("FAIL analysis::workload_00 [case_000]: I1mr 0→4, ILmr 0→4")
            );
        }
    }
    Ok(())
}

#[test]
fn left_only_baseline_workload_is_missing_not_new() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 1)?;
    let root = temporary.path().join("candidate");
    write_summary(
        &root,
        "unpaired",
        &left_only(summary(0, [100, 2, 2], [100, 2, 2]))?,
    )?;
    let output = compare(&root, &manifest)?;
    assert_status(&output, 2)?;
    assert!(
        text(&output.stderr)?
            .contains("missing baseline benchmark ID analysis::workload_00 [case_000]")
    );
    Ok(())
}

#[test]
fn paired_candidate_absent_from_manifest_is_rejected() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 1)?;
    let root = temporary.path().join("candidate");
    candidates(&root, 2, 2, None)?;
    let output = compare(&root, &manifest)?;
    assert_status(&output, 2)?;
    assert!(text(&output.stderr)?.contains("candidate benchmark is paired but absent from baseline manifest: analysis::workload_00 [case_001]"));
    Ok(())
}

#[test]
fn malformed_new_candidate_counts_are_not_excluded() -> TestResult {
    for count in [json!(-1), json!(true), json!(2.0), json!("2"), Value::Null] {
        let temporary = TempDir::new()?;
        let manifest = baseline(temporary.path(), 1)?;
        let root = temporary.path().join("candidate");
        candidates(&root, 1, 1, None)?;
        let mut data = left_only(summary(1, [100, 2, 2], [100, 2, 2]))?;
        metrics(&mut data)?["Ir"]["metrics"]["Left"]["Int"] = count;
        write_summary(&root, "new-malformed", &data)?;
        let output = compare(&root, &manifest)?;
        assert_status(&output, 2)?;
        assert!(text(&output.stderr)?.contains("missing paired counts for Ir"));
    }
    Ok(())
}

#[test]
fn manifest_schema_empty_and_duplicate_ids_are_rejected() -> TestResult {
    for (data, expected) in [
        (
            json!({"format_version": true, "benchmarks": []}),
            "baseline manifest must use format_version 1 and list benchmarks",
        ),
        (
            json!({"format_version": 1.0, "benchmarks": []}),
            "baseline manifest must use format_version 1 and list benchmarks",
        ),
        (
            json!({"format_version": 2, "benchmarks": []}),
            "baseline manifest must use format_version 1 and list benchmarks",
        ),
        (
            json!({"format_version": 1, "benchmarks": {}}),
            "baseline manifest must use format_version 1 and list benchmarks",
        ),
        (
            json!({"format_version": 1, "benchmarks": []}),
            "baseline manifest contains no benchmarks",
        ),
        (
            json!({"format_version": 1, "benchmarks": [{"function_name": "f", "id": null}, {"function_name": "f"}]}),
            "duplicate baseline benchmark ID f",
        ),
        (
            json!({"format_version": 1, "benchmarks": [{"function_name": ""}]}),
            "benchmark function_name must be a non-empty string",
        ),
        (
            json!({"format_version": 1, "benchmarks": [{"function_name": "f", "id": ""}]}),
            "benchmark id must be null or a non-empty string",
        ),
    ] {
        let temporary = TempDir::new()?;
        let manifest = temporary.path().join("manifest.json");
        fs::write(&manifest, serde_json::to_vec(&data)?)?;
        let output = compare(temporary.path(), &manifest)?;
        assert_status(&output, 2)?;
        assert!(text(&output.stderr)?.contains(expected));
    }
    Ok(())
}

#[test]
fn manifest_preserves_exact_identity_and_canonical_order() -> TestResult {
    let temporary = TempDir::new()?;
    let root = temporary.path().join("summaries");
    for (filename, function, id) in [
        ("z", "z", Some("case")),
        ("a2", "a", Some("id")),
        ("a1", "a", None),
    ] {
        let mut data = summary(0, [100, 2, 2], [100, 2, 2]);
        data["function_name"] = json!(function);
        data["id"] = json!(id);
        write_summary(&root, filename, &data)?;
    }
    let manifest = temporary.path().join("nested/manifest.json");
    let output = cli(
        &root,
        &[format!("--write-baseline-manifest={}", manifest.display())],
    )?;
    assert_status(&output, 0)?;
    let bytes = fs::read(&manifest)?;
    assert!(bytes.ends_with(b"\n"));
    let data: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(
        data,
        json!({"format_version": 1, "benchmarks": [{"function_name":"a","id":null}, {"function_name":"a","id":"id"}, {"function_name":"z","id":"case"}]})
    );
    Ok(())
}

#[test]
fn malformed_stale_and_empty_summary_trees_fail() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 1)?;
    let root = temporary.path().join("candidate");
    fs::create_dir_all(&root)?;
    let empty = compare(&root, &manifest)?;
    assert_status(&empty, 2)?;
    assert!(text(&empty.stderr)?.contains("missing baseline benchmark ID"));
    let mut stale = summary(0, [100, 2, 2], [100, 2, 2]);
    stale["baselines"] = json!(["main", null]);
    write_summary(&root, "stale", &stale)?;
    let stale_result = compare(&root, &manifest)?;
    assert_status(&stale_result, 2)?;
    assert!(text(&stale_result.stderr)?.contains("missing baseline benchmark ID"));
    fs::write(root.join("summary.json"), b"{broken")?;
    assert_status(&compare(&root, &manifest)?, 2)?;
    let empty_root = temporary.path().join("empty");
    fs::create_dir_all(&empty_root)?;
    let output = cli(
        &empty_root,
        &[format!(
            "--write-baseline-manifest={}",
            temporary.path().join("empty-manifest.json").display()
        )],
    )?;
    assert_status(&output, 2)?;
    assert!(text(&output.stderr)?.contains("no Iai summary.json files found"));
    Ok(())
}

#[test]
fn prior_cli_flags_modes_and_help_are_preserved() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 1)?;
    let root = temporary.path().join("candidate");
    candidates(&root, 1, 1, None)?;
    for arguments in [
        vec![],
        vec![format!("--baseline-manifest={}", manifest.display())],
        vec![
            format!("--baseline-manifest={}", manifest.display()),
            "--write-baseline-manifest=other.json".into(),
        ],
        vec!["--unexpected".into()],
    ] {
        assert_status(&cli(&root, &arguments)?, 2)?;
    }
    let output = cli(
        &root,
        &[
            "--baseline-manifest".into(),
            manifest.display().to_string(),
            "--baseline-name".into(),
            "selected".into(),
        ],
    )?;
    assert_status(&output, 0)?;
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["policy", "--help"])
        .output()?;
    assert_status(&output, 0)?;
    assert!(text(&output.stdout)?.contains("--write-baseline-manifest"));
    assert!(text(&output.stdout)?.contains("--baseline-name"));
    assert!(output.stderr.is_empty());
    let output = cli(
        &temporary.path().join("absent"),
        &[
            format!("--baseline-manifest={}", manifest.display()),
            "--baseline-name=selected".into(),
        ],
    )?;
    assert_status(&output, 2)?;
    assert!(text(&output.stderr)?.contains("summary root is not a directory"));
    Ok(())
}

#[test]
fn retained_verdict_uses_the_authoritative_comparison_and_preserves_cli_failures() -> TestResult {
    let temporary = TempDir::new()?;
    let manifest = baseline(temporary.path(), 48)?;
    let root = temporary.path().join("candidate");
    for index in 0..48 {
        write_summary(
            &root,
            &format!("candidate-{index:03}"),
            &summary(
                index,
                [if index < 5 { 103 } else { 100 }, 2, 2],
                [100, 2, 2],
            ),
        )?;
    }
    let artifact_manifest = temporary.path().join("artifact-manifest.json");
    fs::write(
        &artifact_manifest,
        serde_json::to_vec(&json!({
            "schema_version":1,"mode":"callgrind","target":"x86_64-unknown-linux-gnu",
            "base_sha":"a".repeat(40),"candidate_sha":"b".repeat(40),"workflow_sha":"c".repeat(40),
            "repository":"owner/repository","request_id":"fixture-run-0001",
            "run_id":"12345","run_attempt":"1"
        }))?,
    )?;
    let verdict = temporary.path().join("callgrind-policy.json");
    let arguments = [
        "--baseline-name=selected".to_owned(),
        format!("--baseline-manifest={}", manifest.display()),
        format!("--artifact-manifest={}", artifact_manifest.display()),
        format!("--verdict={}", verdict.display()),
    ];
    let output = cli(&root, &arguments)?;
    assert_status(&output, 1)?;
    assert!(output.stderr.is_empty());
    assert!(text(&output.stdout)?.contains("43/48 baseline workloads passed."));
    let retained: Value = serde_json::from_slice(&fs::read(&verdict)?)?;
    assert_eq!(retained["status"], "regression");
    assert_eq!(retained["passed"], false);
    assert_eq!(retained["passed_count"], 43);
    assert_eq!(retained["workload_count"], 48);
    assert_eq!(retained["workloads"][0]["metrics"]["Ir"]["baseline"], 100);
    assert_eq!(retained["workloads"][0]["metrics"]["Ir"]["candidate"], 103);
    assert_eq!(retained["workloads"][0]["failed_metrics"], json!(["Ir"]));
    assert_eq!(
        retained["inputs"]["summaries"]
            .as_array()
            .ok_or("Missing retained inventory")?
            .len(),
        48
    );
    fs::write(root.join("candidate-000/summary.json"), b"{}")?;
    let output = cli(&root, &arguments)?;
    assert_status(&output, 2)?;
    let failed: Value = serde_json::from_slice(&fs::read(&verdict)?)?;
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["passed"], false);
    assert!(failed["workloads"].is_null());
    Ok(())
}

fn admission_bundle(regression_metric: Option<usize>) -> TestResult<TempDir> {
    let temporary = TempDir::new()?;
    let root = temporary.path();
    let baseline = baseline(root, 1)?;
    let mut counts = [100, 2, 2];
    if let Some(metric) = regression_metric {
        counts[metric] = if metric == 0 { 103 } else { 6 };
    }
    let summary_root = root.join("candidate");
    write_summary(
        &summary_root,
        "candidate-000",
        &summary(0, counts, [100, 2, 2]),
    )?;
    let manifest_path = root.join("artifact-manifest.json");
    let mut manifest = json!({
        "schema_version":1, "mode":"callgrind", "target":"x86_64-unknown-linux-gnu",
        "base_sha":"a".repeat(40), "candidate_sha":"b".repeat(40),
        "workflow_sha":"c".repeat(40), "repository":"owner/repository",
        "request_id":"fixture-run-0001", "run_id":"12345", "run_attempt":"1",
        "gate_identity":{"workflow":"performance", "job":"compare",
            "step":"Compare pull request with main", "head_sha":"d".repeat(40)}
    });
    fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
    let output = cli(
        &summary_root,
        &[
            "--baseline-name=selected".into(),
            format!("--baseline-manifest={}", baseline.display()),
            format!("--artifact-manifest={}", manifest_path.display()),
            format!("--verdict={}", root.join("callgrind-policy.json").display()),
        ],
    )?;
    assert_status(&output, i32::from(regression_metric.is_some()))?;
    let names = [
        "baseline-manifest.json",
        "candidate/candidate-000/summary.json",
        "callgrind-policy.json",
    ];
    manifest["files"] = Value::Array(
        names
            .iter()
            .map(|name| artifact_identity(root, name))
            .collect::<TestResult<_>>()?,
    );
    manifest["expected"] = json!(names.map(|name| json!({
        "path":name, "required":true, "present":true
    })));
    manifest["statuses"] = json!({
        "baseline":{"status":"success"}, "callgrind-data":{"status":"success"},
        "candidate":{
            "status":if regression_metric.is_some() { "failure" } else { "success" },
            "benchmark_exit_code":if regression_metric.is_some() { 3 } else { 0 },
            "policy_exit_code":u8::from(regression_metric.is_some())
        }
    });
    fs::write(manifest_path, serde_json::to_vec(&manifest)?)?;
    Ok(temporary)
}

fn artifact_identity(root: &Path, name: &str) -> TestResult<Value> {
    use sha2::Digest as _;
    let bytes = fs::read(root.join(name))?;
    Ok(json!({
        "path":name, "sha256":format!("{:x}", sha2::Sha256::digest(&bytes)), "size":bytes.len()
    }))
}

fn admission(
    root: &Path,
    benchmark_code: u8,
    policy_code: u8,
    extra: &[String],
) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["admission", "measurement", "--root"])
        .arg(root)
        .args([
            format!("--base-sha={}", "a".repeat(40)),
            format!("--candidate-sha={}", "b".repeat(40)),
            format!("--head-sha={}", "d".repeat(40)),
            "--run-id=12345".into(),
            format!("--benchmark-exit-code={benchmark_code}"),
            format!("--policy-exit-code={policy_code}"),
        ])
        .args(extra)
        .output()?)
}

fn mutate_json(path: &Path, change: impl FnOnce(&mut Value)) -> TestResult {
    let mut value: Value = serde_json::from_slice(&fs::read(path)?)?;
    change(&mut value);
    fs::write(path, serde_json::to_vec(&value)?)?;
    Ok(())
}

fn refresh_admission_identity(root: &Path, name: &str) -> TestResult {
    let identity = artifact_identity(root, name)?;
    let manifest_path = root.join("artifact-manifest.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let item = manifest["files"]
        .as_array_mut()
        .ok_or("Missing fixture file inventory")?
        .iter_mut()
        .find(|item| item["path"] == name)
        .ok_or("Missing fixture identity")?;
    *item = identity;
    fs::write(manifest_path, serde_json::to_vec(&manifest)?)?;
    Ok(())
}

fn assert_admission_rejected(output: &Output) -> TestResult {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        text(&output.stdout)?
    );
    assert!(
        output.stdout.is_empty(),
        "classification emitted before validation"
    );
    assert!(!output.stderr.is_empty());
    Ok(())
}

#[test]
fn admission_preserves_real_pass_and_each_metric_regression() -> TestResult {
    for metric in [None, Some(0), Some(1), Some(2)] {
        let temporary = admission_bundle(metric)?;
        let output = admission(
            temporary.path(),
            if metric.is_some() { 3 } else { 0 },
            u8::from(metric.is_some()),
            &[],
        )?;
        assert_status(&output, 0)?;
        assert!(output.stderr.is_empty());
        let classified: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            classified["status"],
            if metric.is_some() {
                "regression"
            } else {
                "passed"
            }
        );
        assert_eq!(classified["base_sha"], "a".repeat(40));
        assert_eq!(classified["candidate_sha"], "b".repeat(40));
        assert_eq!(classified["head_sha"], "d".repeat(40));
        assert_eq!(classified["run_id"], "12345");
        let failures = classified["failures"]
            .as_array()
            .ok_or("Missing failure evidence")?;
        assert_eq!(failures.len(), usize::from(metric.is_some()));
        if let Some(metric) = metric {
            assert!(
                failures[0]
                    .as_str()
                    .is_some_and(|failure| failure.contains(METRICS[metric]))
            );
            mutate_json(
                &temporary.path().join("artifact-manifest.json"),
                |manifest| {
                    manifest["statuses"]["candidate"]["benchmark_exit_code"] = json!(0);
                },
            )?;
            assert_status(&admission(temporary.path(), 0, 1, &[])?, 0)?;
        }
    }
    Ok(())
}

#[test]
fn admission_rejects_mismatched_source_head_run_and_verdict_provenance() -> TestResult {
    for field in ["base_sha", "candidate_sha", "run_id", "head_sha"] {
        let temporary = admission_bundle(None)?;
        mutate_json(
            &temporary.path().join("artifact-manifest.json"),
            |manifest| {
                if field == "head_sha" {
                    manifest["gate_identity"][field] = json!("e".repeat(40));
                } else {
                    manifest[field] = json!(if field == "run_id" {
                        "99999".into()
                    } else {
                        "e".repeat(40)
                    });
                }
            },
        )?;
        assert_admission_rejected(&admission(temporary.path(), 0, 0, &[])?)?;
    }
    let temporary = admission_bundle(None)?;
    mutate_json(&temporary.path().join("callgrind-policy.json"), |verdict| {
        verdict["provenance"]["run_id"] = json!("99999");
    })?;
    refresh_admission_identity(temporary.path(), "callgrind-policy.json")?;
    assert_admission_rejected(&admission(temporary.path(), 0, 0, &[])?)?;
    Ok(())
}

#[test]
fn admission_rejects_inconsistent_exits_and_collector_errors() -> TestResult {
    for (regression, benchmark, policy) in [
        (false, 3, 0),
        (false, 0, 1),
        (false, 101, 0),
        (true, 3, 0),
        (true, 3, 2),
        (true, 101, 1),
    ] {
        let temporary = admission_bundle(regression.then_some(0))?;
        assert_admission_rejected(&admission(temporary.path(), benchmark, policy, &[])?)?;
    }
    for mutation in [
        "exits",
        "candidate-status",
        "baseline",
        "missing-status",
        "collector",
    ] {
        let temporary = admission_bundle(Some(0))?;
        mutate_json(
            &temporary.path().join("artifact-manifest.json"),
            |manifest| match mutation {
                "exits" => manifest["statuses"]["candidate"]["benchmark_exit_code"] = json!(0),
                "candidate-status" => {
                    manifest["statuses"]["candidate"]["status"] = json!("success");
                }
                "baseline" => manifest["statuses"]["baseline"]["status"] = json!("failure"),
                "missing-status" => manifest["statuses"] = Value::Null,
                _ => manifest["statuses"]["collector"] = json!({"status":"failure"}),
            },
        )?;
        assert_admission_rejected(&admission(temporary.path(), 3, 1, &[])?)?;
    }
    Ok(())
}

#[test]
fn admission_rejects_missing_malformed_tampered_and_forged_evidence() -> TestResult {
    for name in [
        "artifact-manifest.json",
        "callgrind-policy.json",
        "candidate/candidate-000/summary.json",
    ] {
        for missing in [true, false] {
            let temporary = admission_bundle(None)?;
            if missing {
                fs::remove_file(temporary.path().join(name))?;
            } else {
                fs::write(temporary.path().join(name), b"{broken")?;
            }
            assert_admission_rejected(&admission(temporary.path(), 0, 0, &[])?)?;
        }
    }
    let temporary = admission_bundle(Some(0))?;
    mutate_json(&temporary.path().join("callgrind-policy.json"), |verdict| {
        verdict["workloads"][0]["metrics"]["Ir"]["candidate"] = json!(100);
    })?;
    refresh_admission_identity(temporary.path(), "callgrind-policy.json")?;
    assert_admission_rejected(&admission(temporary.path(), 3, 1, &[])?)?;
    for mutation in ["identity", "duplicate", "escape", "missing-input", "size"] {
        let temporary = admission_bundle(None)?;
        mutate_json(
            &temporary.path().join("artifact-manifest.json"),
            |manifest| match mutation {
                "identity" => manifest["files"][0]["sha256"] = json!("0".repeat(64)),
                "duplicate" => manifest["files"][1] = manifest["files"][0].clone(),
                "escape" => manifest["files"][0]["path"] = json!("../baseline-manifest.json"),
                "missing-input" => {
                    manifest["files"][0]["path"] =
                        json!("baseline-summaries/baseline-000/summary.json");
                }
                _ => manifest["files"][0]["size"] = json!(0),
            },
        )?;
        assert_admission_rejected(&admission(temporary.path(), 0, 0, &[])?)?;
    }
    Ok(())
}

fn admission_environment(input: &Path, owner_id: u64) -> TestResult<Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["admission", "environment", "--input"])
        .arg(input)
        .arg(format!("--owner-id={owner_id}"))
        .output()?)
}

#[test]
fn admission_environment_requires_sole_owner_user_review_without_admin_bypass() -> TestResult {
    let temporary = TempDir::new()?;
    let input = temporary.path().join("environment.json");
    let valid = json!({
        "name":"performance-exception", "can_admins_bypass":false,
        "protection_rules":[{"type":"required_reviewers", "prevent_self_review":false,
            "reviewers":[{"type":"User", "reviewer":{"id":668_727}}]}]
    });
    fs::write(&input, serde_json::to_vec(&valid)?)?;
    assert_status(&admission_environment(&input, 668_727)?, 0)?;
    assert_admission_rejected(&admission_environment(&input, 0)?)?;
    assert_admission_rejected(&admission_environment(&input, 1)?)?;
    for mutation in [
        "name",
        "unprotected",
        "missing-rules",
        "wrong-id",
        "team",
        "additional-reviewer",
        "duplicate-rule",
        "bypass",
        "missing-bypass",
    ] {
        let mut environment = valid.clone();
        match mutation {
            "name" => environment["name"] = json!("other"),
            "unprotected" => environment["protection_rules"] = json!([]),
            "missing-rules" => environment["protection_rules"] = Value::Null,
            "wrong-id" => {
                environment["protection_rules"][0]["reviewers"][0]["reviewer"]["id"] = json!(1);
            }
            "team" => environment["protection_rules"][0]["reviewers"][0]["type"] = json!("Team"),
            "additional-reviewer" => {
                environment["protection_rules"][0]["reviewers"] = json!([
                    {"type":"User","reviewer":{"id":668_727}}, {"type":"User","reviewer":{"id":1}}
                ]);
            }
            "duplicate-rule" => {
                environment["protection_rules"] =
                    json!([valid["protection_rules"][0], valid["protection_rules"][0]]);
            }
            "bypass" => environment["can_admins_bypass"] = json!(true),
            _ => environment["can_admins_bypass"] = Value::Null,
        }
        fs::write(&input, serde_json::to_vec(&environment)?)?;
        assert_admission_rejected(&admission_environment(&input, 668_727)?)?;
    }
    fs::write(&input, b"{broken")?;
    assert_admission_rejected(&admission_environment(&input, 668_727)?)?;
    fs::remove_file(&input)?;
    assert_admission_rejected(&admission_environment(&input, 668_727)?)?;
    Ok(())
}

#[test]
fn admission_rejects_coherent_outcome_forgery_even_with_refreshed_hashes() -> TestResult {
    for regression in [false, true] {
        let temporary = admission_bundle(regression.then_some(0))?;
        let forged_pass = regression;
        mutate_json(&temporary.path().join("callgrind-policy.json"), |verdict| {
            verdict["workloads"][0]["metrics"]["Ir"]["passed"] = json!(forged_pass);
            verdict["workloads"][0]["passed"] = json!(forged_pass);
            verdict["workloads"][0]["failed_metrics"] = if forged_pass {
                json!([])
            } else {
                json!(["Ir"])
            };
            verdict["passed"] = json!(forged_pass);
            verdict["passed_count"] = json!(usize::from(forged_pass));
            verdict["status"] = json!(if forged_pass {
                "complete"
            } else {
                "regression"
            });
        })?;
        refresh_admission_identity(temporary.path(), "callgrind-policy.json")?;
        mutate_json(
            &temporary.path().join("artifact-manifest.json"),
            |manifest| {
                manifest["statuses"]["candidate"] = json!({
                    "status":if forged_pass { "success" } else { "failure" },
                    "benchmark_exit_code":if forged_pass { 0 } else { 3 },
                    "policy_exit_code":u8::from(!forged_pass)
                });
            },
        )?;
        assert_admission_rejected(&admission(
            temporary.path(),
            if forged_pass { 0 } else { 3 },
            u8::from(!forged_pass),
            &[],
        )?)?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn admission_rejects_hash_matching_symlinks_outside_the_bundle() -> TestResult {
    let temporary = admission_bundle(None)?;
    let outside = TempDir::new()?;
    let name = "candidate/candidate-000/summary.json";
    let path = temporary.path().join(name);
    let outside_path = outside.path().join("summary.json");
    fs::write(&outside_path, fs::read(&path)?)?;
    fs::remove_file(&path)?;
    std::os::unix::fs::symlink(&outside_path, path)?;
    assert_admission_rejected(&admission(temporary.path(), 0, 0, &[])?)?;
    Ok(())
}
