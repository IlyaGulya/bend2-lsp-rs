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

#[test]
fn native_profile_navigation_is_verified_and_html_escaped() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let trace = "trace <unsafe>#.json";
    fs::write(
        directory.path().join(trace),
        b"actual captured trace fixture",
    )?;
    fs::write(
        directory.path().join("binary"),
        b"symbolized binary fixture",
    )?;
    common::write_json(
        &directory.path().join("profile-manifest.json"),
        &json!({
            "format_version":1,"status":"complete","target":TARGET,"source_sha":"b".repeat(40),
            "backend":"samply","scenario":"discovery-10<script>alert(1)</script>",
            "binary":{"path":"binary","sha256":common::sha256_file(&directory.path().join("binary"))?},"profiler":{"viewer_command":["samply","load",trace]},
            "artifacts":[{"path":trace,"sha256":common::sha256_file(&directory.path().join(trace))?,"kind":"cpu"}],
            "target_pid":42,"scenario_result":{"scenario":"discovery-10<script>alert(1)</script>",
                "status":"complete","pid":42,"binary_sha256":common::sha256_file(&directory.path().join("binary"))?,
                "semantics":{"shutdown":{"clean_exit":true}}},
            "phases":[{"name":"initialized","elapsed_ns":1_000_000}],"errors":[]
        }),
    )?;
    generate(directory.path())?;
    let html = fs::read_to_string(directory.path().join("index.html"))?;
    assert!(html.contains("trace%20%3Cunsafe%3E%23.json"));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(html.contains("samply"));
    assert!(html.contains("no command is run by this page"));
    assert!(!html.contains("<script"));
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

#[test]
fn gate_regression_stays_distinct_from_failed_collection() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    common::write_json(
        &directory.path().join("callgrind.json"),
        &json!({"status":"regression","passed":false}),
    )?;
    manifest(
        directory.path(),
        &json!([{"path":"callgrind.json","kind":"callgrind","required":true}]),
    )?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.status, "regression");
    assert!(report.targets[0].errors.is_empty());
    assert!(report.result().is_err());
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
    common::write_json(
        &gate.join("summary.json"),
        &json!({"format_version":1,
        "benchmarks":{"fixture":{"instructions":1200}}}),
    )?;
    manifest(
        &gate,
        &json!([{"path":"summary.json","kind":"callgrind","required":true}]),
    )?;
    let path = gate.join("artifact-manifest.json");
    let mut value: Value = serde_json::from_reader(fs::File::open(&path)?)?;
    value["mode"] = json!("callgrind");
    value["statuses"] =
        json!({"candidate-gate":{"status":"success"},"baseline-gate":{"status":"success"}});
    common::write_json(&path, &value)?;
    generate(directory.path())?;
    let report = data::collect(&directory.path().canonicalize()?)?;
    assert_eq!(report.targets.len(), 1);
    assert_eq!(report.status, "complete");
    let html = fs::read_to_string(directory.path().join("index.html"))?;
    assert!(html.contains("Canonical Callgrind gates"));
    assert!(html.contains("candidate-gate"));
    assert!(!html.contains("not collected in this bundle"));
    Ok(())
}
