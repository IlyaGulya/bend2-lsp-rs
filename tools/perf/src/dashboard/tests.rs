use super::{data, generate, render};
use crate::{ToolResult, common};
use serde_json::{Value, json};
use std::{fs, path::Path};

const TARGET: &str = "x86_64-unknown-linux-gnu";

fn manifest(root: &Path, expected: &Value) -> ToolResult<()> {
    let mut files = Vec::new();
    for item in expected.as_array().ok_or("Invalid fixture coverage")? {
        let path = root.join(item["path"].as_str().ok_or("Invalid fixture path")?);
        if path.exists() {
            files.push(json!({"path":item["path"],"sha256":common::sha256_file(&path)?,"size":fs::metadata(path)?.len()}));
        }
    }
    common::write_json(
        &root.join("artifact-manifest.json"),
        &json!({
            "schema_version":1,"target":TARGET,"mode":"compare","base_sha":"a".repeat(40),
            "candidate_sha":"b".repeat(40),"expected":expected,"files":files,
            "expected_targets":[TARGET],
            "repository":"owner/repository","request_id":"fixture-run-0001","workflow_sha":"c".repeat(40),
            "run_id":"12345","run_attempt":"1",
            "statuses":{"collector":{"status":"success"}}
        }),
    )
}

fn latency(root: &Path) -> ToolResult<()> {
    let metadata = json!({"target":TARGET,"platform":"linux","machine":"x86_64", "rounds":2,
        "harness_sha256":"1".repeat(64),"fixture_sha256":"2".repeat(64),"samples":3,"warmup":1,
        "binary_sha256":"3".repeat(64),"collection_status":"complete"});
    let mut workloads = serde_json::Map::new();
    for name in [
        "hover_warm",
        "definition_warm",
        "completion_warm",
        "open_to_hover_large",
        "edit_to_hover_large",
        "hover_during_large_edit",
        "hover_during_large_open",
    ] {
        workloads.insert(name.into(), json!({
            "baseline":{"p50_ns":1_000_000,"p95_ns":2_000_000,"rounds":[{"p50_ns":900_000,"p95_ns":1_900_000},{"p50_ns":1_100_000,"p95_ns":2_100_000}]},
            "candidate":{"p50_ns":2_000_000,"p95_ns":3_000_000,"rounds":[{"p50_ns":1_900_000,"p95_ns":2_900_000},{"p50_ns":2_100_000,"p95_ns":3_100_000}]},
            "delta":{"p50_percent":100,"p95_percent":50}
        }));
    }
    common::write_json(
        &root.join("latency-report.json"),
        &json!({"format_version":1,"mode":"report-only", "workload_digest":"4".repeat(64), "baseline_metadata":metadata,"candidate_metadata":metadata,"workloads":workloads}),
    )?;
    manifest(
        root,
        &json!([{"path":"latency-report.json","kind":"latency","required":true}]),
    )
}

#[test]
fn latency_report_retains_actual_values_rounds_and_offline_navigation() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    latency(directory.path())?;
    generate(directory.path())?;
    let html = fs::read_to_string(directory.path().join("index.html"))?;
    assert!(html.contains("1.000 / 2.000"));
    assert!(html.contains("2.000 / 3.000"));
    assert!(html.contains("0.900"));
    assert!(html.contains("round distributions"));
    assert!(html.contains("./latency-report.json"));
    assert!(!html.contains("<script"));
    let report: Value = serde_json::from_reader(fs::File::open(
        directory.path().join("unified-report.json"),
    )?)?;
    assert_eq!(report["status"], "complete");
    assert_eq!(
        report["targets"][0]["documents"][0]["data"]["workloads"]["hover_warm"]["candidate"]["p95_ns"],
        3_000_000
    );
    let markdown = fs::read_to_string(directory.path().join("summary.md"))?;
    assert!(markdown.contains("2.000 / 3.000"));
    Ok(())
}

#[test]
fn missing_required_evidence_writes_diagnostic_surface_and_fails() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    manifest(
        directory.path(),
        &json!([{"path":"memory/report.json","kind":"memory","required":true}]),
    )?;
    assert!(generate(directory.path()).is_err());
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "incomplete");
    assert!(fs::read_to_string(directory.path().join("index.html"))?.contains("memory"));
    Ok(())
}

#[test]
fn tampered_measurement_is_not_reported_as_a_success() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    latency(directory.path())?;
    fs::write(directory.path().join("latency-report.json"), b"{}")?;
    assert!(generate(directory.path()).is_err());
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "failed");
    assert!(
        report.targets[0]
            .errors
            .iter()
            .any(|error| error.contains("SHA256 mismatch"))
    );
    Ok(())
}

#[test]
fn aggregate_requires_expected_targets_and_exact_source_pairing() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    latency(directory.path())?;
    common::write_json(
        &directory.path().join("run-manifest.json"),
        &json!({"format_version":1,
        "mode":"compare","base_sha":"a".repeat(40),"candidate_sha":"c".repeat(40),
        "expected_targets":[TARGET,"aarch64-apple-darwin"]}),
    )?;
    assert!(generate(directory.path()).is_err());
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.contains("Expected target artifact is absent"))
    );
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("candidate_sha"))
    );
    Ok(())
}

fn navigation_profile(root: &Path, status: &str) -> ToolResult<()> {
    let trace = "trace &unsafe#.json";
    fs::write(root.join(trace), b"actual captured trace fixture")?;
    fs::write(root.join("binary"), b"symbolized binary fixture")?;
    common::write_json(
        &root.join("profile-manifest.json"),
        &json!({
            "format_version":1,"status":status,"target":TARGET,"source_sha":"b".repeat(40),
            "backend":"samply","scenario":"discovery-10<script>alert(1)</script>",
            "binary":{"path":"binary","sha256":common::sha256_file(&root.join("binary"))?},"profiler":{"viewer_command":["samply","load",trace]},
            "artifacts":[{"path":trace,"sha256":common::sha256_file(&root.join(trace))?,"kind":"cpu"}],
            "target_pid":42,"scenario_result":{"scenario":"discovery-10<script>alert(1)</script>",
                "status":"complete","pid":42,"binary_sha256":common::sha256_file(&root.join("binary"))?,
                "semantics":{"shutdown":{"clean_exit":true}}},
            "phases":[{"name":"initialized","elapsed_ns":1_000_000}],"errors":[],"error":"fixture capture failed"
        }),
    )
}

#[test]
fn native_profile_navigation_is_verified_and_html_escaped() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    navigation_profile(directory.path(), "complete")?;
    manifest(
        directory.path(),
        &json!([{"path":"profile-manifest.json","kind":"profile","required":true}]),
    )?;
    generate(directory.path())?;
    let html = fs::read_to_string(directory.path().join("index.html"))?;
    assert!(html.contains("trace%20%26unsafe%23.json"));
    assert!(html.contains("trace &amp;unsafe#.json"));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(html.contains("samply"));
    assert!(html.contains("no command is run by this page"));
    assert!(!html.contains("<script"));
    Ok(())
}

#[test]
fn standalone_profile_outcome_is_derived_from_validated_retained_capture() -> ToolResult<()> {
    for (capture_status, expected_status) in [
        ("complete", "complete"),
        ("failed", "failed"),
        ("collecting", "incomplete"),
        ("unknown", "incomplete"),
    ] {
        let directory = tempfile::tempdir()?;
        navigation_profile(directory.path(), capture_status)?;
        let report = data::collect(&directory.path().canonicalize()?)?;
        assert_eq!(report.status, expected_status, "{capture_status}");
        assert_eq!(
            report.targets[0].provenance["statuses"]["profile-capture"]["status"],
            capture_status
        );
        assert_eq!(report.result().is_ok(), capture_status == "complete");
        if capture_status == "failed" {
            assert!(
                report.targets[0]
                    .errors
                    .iter()
                    .any(|error| error.contains("fixture capture failed"))
            );
        }
    }
    let directory = tempfile::tempdir()?;
    navigation_profile(directory.path(), "complete")?;
    fs::write(
        directory.path().join("trace &unsafe#.json"),
        b"changed retained trace",
    )?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "failed");
    assert!(report.result().is_err());
    Ok(())
}

#[test]
fn artifact_paths_cannot_traverse_or_turn_into_viewer_urls() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    for path in [
        "../other",
        "/etc/passwd",
        "a/../../other",
        "a\\..\\other",
        "https://example.com/trace",
    ] {
        assert!(data::confined(&root, &root, path).is_err());
    }
    assert_eq!(render::escape("<\"&'>"), "&lt;&quot;&amp;&#39;&gt;");
    Ok(())
}

fn policy_bundle(root: &Path, workload_count: usize, regression_count: usize) -> ToolResult<()> {
    let summaries = root.join("analysis_hot_paths");
    fs::create_dir_all(&summaries)?;
    let mut expected = vec![
        json!({"path":"analysis_hot_paths-baseline.json","kind":"callgrind-input","required":true}),
        json!({"path":"callgrind-policy.json","kind":"callgrind","required":true}),
    ];
    for index in 0..workload_count {
        let name = format!("workload-{index:03}");
        let directory = summaries.join(&name);
        fs::create_dir(&directory)?;
        let candidate = if index < regression_count { 1021 } else { 1000 };
        common::write_json(
            &directory.join("summary.json"),
            &json!({
                "function_name":"analyze", "id":name, "baselines":[null,"main"],
                "profiles":[{"summaries":{"parts":[{"metrics_summary":{"Callgrind":{
                    "Ir":{"metrics":{"Both":[{"Int":candidate},{"Int":1000}]}},
                    "I1mr":{"metrics":{"Both":[{"Int":100},{"Int":100}]}},
                    "ILmr":{"metrics":{"Both":[{"Int":100},{"Int":100}]}}
                }}}]}}]
            }),
        )?;
        expected.push(
            json!({"path":format!("analysis_hot_paths/{name}/summary.json"),
            "kind":"callgrind-input","required":true}),
        );
    }
    let baseline = root.join("analysis_hot_paths-baseline.json");
    crate::policy::run(&[
        summaries.display().to_string(),
        format!("--write-baseline-manifest={}", baseline.display()),
    ])?;
    let expected = Value::Array(expected);
    manifest(root, &expected)?;
    let path = root.join("artifact-manifest.json");
    let mut value: Value = serde_json::from_reader(fs::File::open(&path)?)?;
    value["mode"] = json!("callgrind");
    value["gate_identity"] = json!({"workflow":"performance","job":"compare",
        "step":"Compare pull request with main","head_sha":"d".repeat(40)});
    common::write_json(&path, &value)?;
    let outcome = crate::policy::run(&[
        summaries.display().to_string(),
        format!("--baseline-manifest={}", baseline.display()),
        "--baseline-name=main".into(),
        format!("--artifact-manifest={}", path.display()),
        format!("--verdict={}", root.join("callgrind-policy.json").display()),
    ]);
    if regression_count == 0 {
        outcome?;
    } else {
        let error = outcome
            .err()
            .ok_or("Regression producer unexpectedly succeeded")?;
        assert_eq!(
            error
                .downcast_ref::<crate::policy::ExitError>()
                .map(|error| error.status),
            Some(1)
        );
    }
    manifest(root, &expected)?;
    let mut value: Value = serde_json::from_reader(fs::File::open(&path)?)?;
    value["mode"] = json!("callgrind");
    value["gate_identity"] = json!({"workflow":"performance","job":"compare",
        "step":"Compare pull request with main","head_sha":"d".repeat(40)});
    value["statuses"] = json!({
        "baseline":{"status":"success"},"callgrind-data":{"status":"success"},
        "candidate":{"status":if regression_count == 0 {"success"} else {"failure"},
            "benchmark_exit_code":if regression_count == 0 {0} else {3},
            "policy_exit_code":i32::from(regression_count != 0)}
    });
    common::write_json(&path, &value)
}

#[test]
fn gate_regression_stays_distinct_from_failed_collection() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    policy_bundle(directory.path(), 48, 5)?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "regression");
    assert!(report.targets[0].errors.is_empty());
    assert!(report.result().is_err());
    assert_eq!(report.targets[0].regressions.len(), 5);
    assert!(report.targets[0].regressions[0].contains("Ir 1000→1021"));
    let verdict = &report.targets[0].documents[0].data;
    assert_eq!(verdict["workload_count"], 48);
    assert_eq!(verdict["passed_count"], 43);
    assert_eq!(verdict["workloads"][0]["metrics"]["Ir"]["candidate"], 1021);
    assert!(generate(directory.path()).is_err());
    Ok(())
}

fn refresh_identity(root: &Path, name: &str) -> ToolResult<()> {
    let path = root.join("artifact-manifest.json");
    let mut value: Value = serde_json::from_reader(fs::File::open(&path)?)?;
    let identity = value["files"]
        .as_array_mut()
        .ok_or("Missing fixture inventory")?
        .iter_mut()
        .find(|item| item["path"] == name)
        .ok_or("Missing fixture identity")?;
    identity["sha256"] = json!(common::sha256_file(&root.join(name))?);
    identity["size"] = json!(fs::metadata(root.join(name))?.len());
    common::write_json(&path, &value)
}

#[test]
fn completed_policy_pass_requires_real_retained_numeric_evidence() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    policy_bundle(directory.path(), 48, 0)?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "complete");
    assert!(report.targets[0].regressions.is_empty());
    assert_eq!(report.targets[0].documents[0].data["passed_count"], 48);
    report.result()?;
    Ok(())
}

#[test]
fn unrelated_collection_failure_takes_precedence_over_policy_regression() -> ToolResult<()> {
    for (name, value) in [
        ("baseline", json!({"status":"failure"})),
        ("other-collector", json!({"status":"failure"})),
        (
            "candidate",
            json!({"status":"failure","benchmark_exit_code":101,"policy_exit_code":1}),
        ),
        ("candidate", json!({"status":"failure"})),
    ] {
        let directory = tempfile::tempdir()?;
        policy_bundle(directory.path(), 48, 5)?;
        let path = directory.path().join("artifact-manifest.json");
        let mut manifest: Value = serde_json::from_reader(fs::File::open(&path)?)?;
        manifest["statuses"][name] = value;
        common::write_json(&path, &manifest)?;
        let report = data::collect(&directory.path().canonicalize()?)?;
        assert_eq!(report.status, "failed");
        assert_eq!(report.targets[0].regressions.len(), 5);
        assert!(report.result().is_err());
    }
    Ok(())
}

#[test]
fn missing_old_policy_verdict_never_infers_a_passing_gate() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    policy_bundle(directory.path(), 48, 0)?;
    let path = directory.path().join("artifact-manifest.json");
    let mut manifest: Value = serde_json::from_reader(fs::File::open(&path)?)?;
    manifest["expected"]
        .as_array_mut()
        .ok_or("Missing fixture coverage")?
        .retain(|item| item["path"] != "callgrind-policy.json");
    manifest["files"]
        .as_array_mut()
        .ok_or("Missing fixture files")?
        .retain(|item| item["path"] != "callgrind-policy.json");
    common::write_json(&path, &manifest)?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "incomplete");
    assert!(report.targets[0].regressions.is_empty());
    assert!(report.result().is_err());
    Ok(())
}

#[test]
fn malformed_source_or_numeric_verdict_cannot_classify_a_failed_gate_as_regression()
-> ToolResult<()> {
    for mutation in [
        "numeric",
        "source",
        "missing-workload",
        "outcome",
        "failed-collection",
    ] {
        let directory = tempfile::tempdir()?;
        policy_bundle(directory.path(), 48, 5)?;
        let path = directory.path().join("callgrind-policy.json");
        let mut verdict: Value = serde_json::from_reader(fs::File::open(&path)?)?;
        match mutation {
            "numeric" => verdict["workloads"][0]["metrics"]["Ir"]["candidate"] = json!(1022),
            "source" => verdict["provenance"]["candidate_sha"] = json!("d".repeat(40)),
            "missing-workload" => {
                verdict["workloads"]
                    .as_array_mut()
                    .ok_or("Missing fixture workloads")?
                    .pop();
            }
            "outcome" => verdict["passed"] = json!(true),
            "failed-collection" => {
                verdict["status"] = json!("failed");
                verdict["error"] = json!("candidate collection failed");
            }
            _ => return Err("Unknown fixture mutation".into()),
        }
        common::write_json(&path, &verdict)?;
        refresh_identity(directory.path(), "callgrind-policy.json")?;
        let report = data::collect(&directory.path().canonicalize()?)?;
        assert_eq!(report.status, "failed", "{mutation}");
        assert!(report.targets[0].regressions.is_empty(), "{mutation}");
        assert!(report.result().is_err());
    }
    Ok(())
}

#[test]
fn verdict_inputs_are_bound_to_raw_artifact_integrity() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    policy_bundle(directory.path(), 48, 5)?;
    let name = "analysis_hot_paths/workload-000/summary.json";
    let path = directory.path().join(name);
    let mut summary: Value = serde_json::from_reader(fs::File::open(&path)?)?;
    summary["profiles"][0]["summaries"]["parts"][0]["metrics_summary"]["Callgrind"]["Ir"]["metrics"]
        ["Both"][0]["Int"] = json!(2000);
    common::write_json(&path, &summary)?;
    // Updating the outer bundle inventory does not repair the evaluator's
    // independently retained identity of the actual comparison input.
    refresh_identity(directory.path(), name)?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "failed");
    assert!(report.targets[0].regressions.is_empty());
    assert!(
        report.targets[0]
            .errors
            .iter()
            .any(|error| error.contains("input identity"))
    );
    Ok(())
}

fn workflow_failure() -> Value {
    json!({"format_version":1,"repository":"owner/repository","run":{
        "databaseId":12345,"attempt":1,"headSha":"d".repeat(40),"event":"pull_request",
        "status":"completed","conclusion":"failure","workflowName":"performance",
        "url":"https://github.com/owner/repository/actions/runs/12345",
        "jobs":[
            {"databaseId":1,"name":"compare","status":"completed","conclusion":"failure",
                "steps":[{"number":1,"name":"Measure main baseline","status":"completed","conclusion":"success"},
                    {"number":2,"name":"Compare pull request with main","status":"completed","conclusion":"failure"}]},
            {"databaseId":2,"name":"native performance","status":"completed","conclusion":"success",
                "steps":[{"number":1,"name":"Collect native measurements","status":"completed","conclusion":"success"}]}
        ]
    }})
}

#[test]
fn hosted_failure_is_a_regression_only_when_every_failed_job_and_step_is_the_bound_gate()
-> ToolResult<()> {
    for mutation in [
        "none",
        "other-job",
        "other-step",
        "unknown-step",
        "missing-steps",
        "wrong-head",
        "wrong-run",
        "wrong-attempt",
    ] {
        let directory = tempfile::tempdir()?;
        policy_bundle(directory.path(), 48, 5)?;
        let mut evidence = workflow_failure();
        match mutation {
            "none" => {}
            "other-job" => {
                evidence["run"]["jobs"][1]["conclusion"] = json!("failure");
                evidence["run"]["jobs"][1]["steps"][0]["conclusion"] = json!("failure");
            }
            "other-step" => {
                evidence["run"]["jobs"][0]["steps"][0]["conclusion"] = json!("failure");
            }
            "unknown-step" => {
                evidence["run"]["jobs"][1]["steps"][0]["conclusion"] = json!("cancelled");
            }
            "missing-steps" => evidence["run"]["jobs"][1]["steps"] = json!([]),
            "wrong-head" => evidence["run"]["headSha"] = json!("e".repeat(40)),
            "wrong-run" => evidence["run"]["databaseId"] = json!(12346),
            "wrong-attempt" => evidence["run"]["attempt"] = json!(2),
            _ => return Err("Unknown workflow fixture mutation".into()),
        }
        common::write_json(&directory.path().join("workflow-run.json"), &evidence)?;
        let report = data::collect(&directory.path().canonicalize()?)?;
        assert_eq!(
            report.status,
            if mutation == "none" {
                "regression"
            } else {
                "failed"
            },
            "{mutation}"
        );
        assert!(report.result().is_err());
        assert_eq!(report.targets[0].regressions.len(), 5);
    }
    Ok(())
}

#[test]
fn historical_workflow_failure_without_job_metadata_is_not_reclassified() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    policy_bundle(directory.path(), 48, 5)?;
    common::write_json(
        &directory.path().join("run-manifest.json"),
        &json!({
            "format_version":1,"mode":"callgrind","base_sha":"a".repeat(40),"candidate_sha":"b".repeat(40),
            "repository":"owner/repository","request_id":"fixture-run-0001",
            "expected_targets":[TARGET],"workflow_succeeded":false
        }),
    )?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "failed");
    assert_eq!(report.targets[0].regressions.len(), 5);
    Ok(())
}

#[test]
fn changed_scope_and_native_memory_timeline_are_not_regressions() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    fs::create_dir(directory.path().join("discovery"))?;
    let metadata = json!({"native_target":TARGET,"baseline_revision":"a".repeat(40),
        "candidate_revision":"b".repeat(40),"rounds":1,"dataset_counts":[10],
        "binaries":{"baseline":{"sha256":"1".repeat(64)},"candidate":{"sha256":"2".repeat(64)}},
        "process_memory":{"units":"bytes","metrics":{"rss_bytes":"Resident pages only; not private commit"}}});
    let observed = json!({"round":1,"status":"complete","samples":[
        {"elapsed_ns":0,"rss_bytes":1_048_576,"kernel_high_watermark_bytes":null},
        {"elapsed_ns":1_000_000_000,"rss_bytes":2_097_152,"native_metrics":{"windows_private_commit_bytes":3_145_728}},
        {"elapsed_ns":2_000_000_000,"rss_bytes":1_048_576}],
        "checkpoints":{"initialize_response":{"elapsed_ns":0,"rss_bytes":1_048_576},
            "initial_completion_response":{"elapsed_ns":200_000_000,"rss_bytes":1_048_576},
            "cold_discovery_observed":{"elapsed_ns":1_000_000_000,"rss_bytes":2_097_152},
            "before_root_removal":{"elapsed_ns":1_500_000_000,"rss_bytes":2_097_152},
            "after_root_removal":{"elapsed_ns":2_000_000_000,"rss_bytes":1_048_576},
            "after_root_removal_settled":{"elapsed_ns":2_100_000_000,"rss_bytes":1_048_576}},
        "metric_definitions":{"windows_private_commit_bytes":"Private committed virtual bytes; not RSS"}});
    let baseline = json!({"completed_rounds":1,"semantic_observations":[{"cold_symbols":{"observed":0,"expected":9,"complete":false}}],
        "cold_medians_ns":{"whole_workspace_symbols_ready":null},"memory_medians_bytes":{"retained_rss_bytes":2_097_152},
        "memory_observations":[observed]});
    let candidate = json!({"completed_rounds":1,"semantic_observations":[{"cold_symbols":{"observed":9,"expected":9,"complete":true}}],
        "cold_medians_ns":{"whole_workspace_symbols_ready":1_000_000},"memory_medians_bytes":{"retained_rss_bytes":2_097_152},
        "memory_observations":[observed]});
    common::write_json(
        &directory.path().join("discovery/report.json"),
        &json!({"format_version":1,
        "status":"complete","metadata":metadata,"datasets":{"10":{"baseline":baseline,"candidate":candidate}}}),
    )?;
    common::write_json(
        &directory.path().join("discovery/raw.json"),
        &json!({"format_version":1,
        "status":"complete","metadata":metadata,"datasets":{"10":{}}}),
    )?;
    manifest(
        directory.path(),
        &json!([{"path":"discovery/report.json","kind":"discovery","required":true},
        {"path":"discovery/raw.json","kind":"discovery-raw","required":true}]),
    )?;
    generate(directory.path())?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "changed-scope");
    assert!(report.targets[0].regressions.is_empty());
    let html = fs::read_to_string(directory.path().join("index.html"))?;
    assert!(html.contains("Peak observed 2.00 MiB"));
    assert!(html.contains("after_root_removal"));
    assert!(html.contains("Private committed virtual bytes; not RSS"));
    assert!(html.contains("unavailable"));
    Ok(())
}

#[test]
fn required_resident_observations_cannot_be_reported_complete() -> ToolResult<()> {
    for resident_available in [false, true] {
        let directory = tempfile::tempdir()?;
        fs::create_dir(directory.path().join("discovery"))?;
        let rss = if resident_available {
            json!(1_048_576)
        } else {
            Value::Null
        };
        let mut checkpoints = serde_json::Map::new();
        for (index, name) in [
            "initialize_response",
            "initial_completion_response",
            "cold_discovery_observed",
            "before_root_removal",
            "after_root_removal",
            "after_root_removal_settled",
        ]
        .into_iter()
        .enumerate()
        {
            checkpoints.insert(
                name.into(),
                json!({
                    "elapsed_ns": index * 100_000_000,
                    "rss_bytes": rss,
                    "kernel_high_watermark_bytes": null,
                }),
            );
        }
        let observation = json!({
            "round": 1, "status": "complete", "checkpoints": checkpoints,
            "samples": [{"elapsed_ns": 0, "rss_bytes": rss, "kernel_high_watermark_bytes": null}],
            "errors": if resident_available { json!([]) } else { json!(["native process memory denied"]) },
        });
        let variant = json!({
            "completed_rounds": 1,
            "semantic_observations": [{"cold_symbols": {"observed": 9, "expected": 9, "complete": true}}],
            "memory_observations": [observation],
        });
        let metadata = json!({
            "native_target": TARGET, "baseline_revision": "a".repeat(40),
            "candidate_revision": "b".repeat(40), "rounds": 1, "dataset_counts": [10],
            "binaries": {"baseline": {"sha256": "1".repeat(64)}, "candidate": {"sha256": "2".repeat(64)}},
        });
        common::write_json(
            &directory.path().join("discovery/report.json"),
            &json!({
                "format_version": 1, "status": "complete", "metadata": metadata,
                "datasets": {"10": {"baseline": variant, "candidate": variant}},
            }),
        )?;
        common::write_json(
            &directory.path().join("discovery/raw.json"),
            &json!({
                "format_version": 1, "status": "complete", "metadata": metadata, "datasets": {"10": {}},
            }),
        )?;
        manifest(
            directory.path(),
            &json!([
                {"path": "discovery/report.json", "kind": "discovery", "required": true},
                {"path": "discovery/raw.json", "kind": "discovery-raw", "required": true},
            ]),
        )?;
        let result = generate(directory.path());
        assert_eq!(result.is_ok(), resident_available);
        let report: Value =
            serde_json::from_slice(&fs::read(directory.path().join("unified-report.json"))?)?;
        assert_eq!(
            report["status"],
            if resident_available {
                "complete"
            } else {
                "incomplete"
            }
        );
    }
    Ok(())
}

#[test]
fn rejected_hosted_request_remains_readable_without_target_artifacts() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    common::write_json(
        &directory.path().join("request.json"),
        &json!({"REQUEST_ID":"rejected-request",
        "MODE":"compare","BASE_SHA":"a".repeat(40),"CANDIDATE_SHA":"b".repeat(40),
        "SELECTED_TARGET":"all","repository":"owner/repository"}),
    )?;
    common::write_json(
        &directory.path().join("validation.json"),
        &json!({"status":"failure",
        "error":"Candidate is not reachable from published source refs"}),
    )?;
    assert!(generate(directory.path()).is_err());
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "failed");
    assert!(
        fs::read_to_string(directory.path().join("index.html"))?
            .contains("Candidate is not reachable")
    );
    Ok(())
}

#[test]
fn target_only_report_never_claims_full_matrix_coverage() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    latency(directory.path())?;
    let path = directory.path().join("artifact-manifest.json");
    let mut value: Value = serde_json::from_reader(fs::File::open(&path)?)?;
    value["expected_targets"] = json!(data::TARGETS);
    common::write_json(&path, &value)?;
    assert!(generate(directory.path()).is_err());
    super::run(&[
        directory
            .path()
            .to_str()
            .ok_or("Non-Unicode fixture")?
            .to_owned(),
        "--target-only".into(),
    ])?;
    let html = fs::read_to_string(directory.path().join("index.html"))?;
    assert!(html.contains("aggregate matrix coverage not evaluated"));
    Ok(())
}

#[test]
fn heap_report_preserves_thirteen_profiles_and_live_allocation_totals() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    fs::create_dir(directory.path().join("memory"))?;
    let mut profiles = Vec::new();
    for index in 0..13 {
        let name = format!("allocation-scenario-{index}");
        let path = directory.path().join("memory").join(&name);
        fs::create_dir(&path)?;
        fs::write(path.join("dhat-heap.json"), b"retained raw DHAT fixture")?;
        profiles.push(json!({"name":name,"status":"complete",
            "total_allocated_bytes":300,"total_allocated_blocks":30,
            "global_peak_live_bytes":90,"global_peak_live_blocks":9,"end_live_bytes":80,"end_live_blocks":16,
            "profile":{"path":"/hosted/old/path/dhat-heap.json","sha256":common::sha256_file(&path.join("dhat-heap.json"))?}}));
    }
    common::write_json(
        &directory.path().join("memory/report.json"),
        &json!({"format_version":1,
        "status":"complete","native_target":TARGET,"profiles":profiles,
        "provenance":{"source":{"baseline_revision":"a".repeat(40),"candidate_revision":"b".repeat(40)}}}),
    )?;
    manifest(
        directory.path(),
        &json!([{"path":"memory/report.json","kind":"memory","required":true}]),
    )?;
    generate(directory.path())?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "complete");
    assert_eq!(report.targets[0].documents[0].links.len(), 13);
    let html = fs::read_to_string(directory.path().join("index.html"))?;
    assert!(html.contains("90.000 / 9.000"));
    assert!(html.contains("80.000 / 16.000"));
    assert!(html.contains("allocation-scenario-12/dhat-heap.json"));
    Ok(())
}

#[test]
fn canonical_gate_bundle_merges_without_a_seventh_or_duplicate_target() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    latency(directory.path())?;
    let gate = directory.path().join("callgrind");
    fs::create_dir(&gate)?;
    policy_bundle(&gate, 48, 5)?;
    assert!(generate(directory.path()).is_err());
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.targets.len(), 1);
    assert_eq!(report.status, "regression");
    let html = fs::read_to_string(directory.path().join("index.html"))?;
    assert!(html.contains("Canonical Callgrind gates"));
    assert!(html.contains("candidate"));
    assert!(html.contains("Ir 1000→1021"));
    assert!(!html.contains("not collected in this bundle"));
    Ok(())
}
