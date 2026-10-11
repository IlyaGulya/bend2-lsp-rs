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
