use super::{assert_error, strict_json};
use crate::{
    ToolResult,
    calibration::{self, Arguments, METRICS, VARIANTS, baseline_name, report},
    common::write_json,
};
use clap::Parser as _;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn dataset(job_id: &str, role: &str, pairs: u64) -> Value {
    let identities = [
        ("inlay_hints_warm", Some("small")),
        ("inlay_hints_warm", Some("medium")),
        ("inlay_hints_warm", Some("large")),
        ("cold_snapshot_build_small", None),
    ];
    let variants: Map<_, _> = VARIANTS.iter().enumerate().map(|(index, name)| ((*name).to_owned(), json!({"source_sha256": if matches!(*name, "a" | "b") { "a".repeat(64) } else { format!("{:x}", index + 1).repeat(64) }, "binary_sha256": format!("{:x}", index + 1).repeat(64), "executable": format!("/isolated/{name}/analysis"), "layout_probe": {"symbol": "analysis::calibration_layout_probe", "address": "1000", "size": if *name == "layout" { 1024 } else { 8 }}, "layout_probe_collected": false}))).collect();
    let samples: Vec<_> = (1..=pairs).flat_map(|pair| VARIANTS.into_iter().enumerate().map(move |(index, name)| json!({"pair": pair, "variant": name, "order": index, "raw_stdout": format!("raw/{pair}-{name}.stdout"), "raw_stderr": format!("raw/{pair}-{name}.stderr"), "workloads": identities.iter().map(|(function, identifier)| json!({"function_name": function, "id": identifier, "counts": {"Ir": if matches!(name, "extra_work" | "extra_alloc") { 110 } else { 100 }, "I1mr": 100, "ILmr": 100}})).collect::<Vec<_>>()}))).collect();
    json!({"format_version": 1, "job_id": job_id, "role": role, "source_revision": "test-revision", "fixture_sha256": "f".repeat(64), "harness_sha256": "e".repeat(64), "environment": {"rustc": "rustc test", "cargo": "cargo test", "iai_runner": "iai test", "valgrind": "valgrind test", "os": "test-linux", "arch": "x86_64", "cache_args": ["--cache-sim=yes"]}, "host": {"runner_identity": job_id, "host_cpu": "test-cpu"}, "variants": variants, "samples": samples})
}

fn counts(
    job: &mut Value,
    variant: &str,
    metric: &str,
    value: u64,
    pair: Option<u64>,
) -> ToolResult<()> {
    for sample in job["samples"].as_array_mut().ok_or("samples missing")? {
        if sample["variant"] == variant && pair.is_none_or(|pair| sample["pair"] == pair) {
            for workload in sample["workloads"]
                .as_array_mut()
                .ok_or("workloads missing")?
            {
                workload["counts"][metric] = value.into();
            }
        }
    }
    Ok(())
}

fn write_job(root: &Path, job: &Value) -> ToolResult<()> {
    fs::create_dir_all(root)?;
    for sample in job["samples"].as_array().ok_or("samples missing")? {
        let path = root.join(sample["raw_stdout"].as_str().ok_or("stdout missing")?);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let baseline = baseline_name(
            job["job_id"].as_str().ok_or("job id missing")?,
            sample["pair"].as_u64().ok_or("pair missing")?,
            sample["variant"].as_str().ok_or("variant missing")?,
        );
        let mut text = String::new();
        for workload in sample["workloads"].as_array().ok_or("workloads missing")? {
            let metrics: Map<_, _> = workload["counts"]
                .as_object()
                .ok_or("counts missing")?
                .iter()
                .map(|(metric, value)| {
                    (metric.clone(), json!({"metrics": {"Left": {"Int": value}}}))
                })
                .collect();
            let summary = json!({"function_name": workload["function_name"], "id": workload["id"], "kind": "LibraryBenchmark", "benchmark_exe": job["variants"][sample["variant"].as_str().ok_or("variant missing")?]["executable"], "baselines": [baseline, baseline], "profiles": [{"tool": "Callgrind", "summaries": {"parts": [{"metrics_summary": {"Callgrind": metrics}}]}}]});
            text.push_str(&serde_json::to_string(&summary)?);
            text.push('\n');
        }
        fs::write(path, text)?;
        let stderr = root.join(sample["raw_stderr"].as_str().ok_or("stderr missing")?);
        if let Some(parent) = stderr.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(stderr, "")?;
    }
    write_json(&root.join("data.json"), job)
}

struct Fixture {
    directory: tempfile::TempDir,
    input: PathBuf,
    training: Value,
    validation: Value,
}

impl Fixture {
    fn new() -> ToolResult<Self> {
        let directory = tempfile::tempdir()?;
        let input = directory.path().join("input");
        Ok(Self {
            directory,
            input,
            training: dataset("training", "discovery", 1),
            validation: dataset("holdout", "validation", 1),
        })
    }
    fn load(&self) -> ToolResult<Vec<Value>> {
        self.load_jobs(&[self.training.clone(), self.validation.clone()])
    }
    fn load_jobs(&self, jobs: &[Value]) -> ToolResult<Vec<Value>> {
        for (index, job) in jobs.iter().enumerate() {
            write_job(&self.input.join(index.to_string()).join("nested"), job)?;
        }
        report::read_datasets(&self.input)
    }
    fn cli(&self, extra: &[&str]) -> ToolResult<()> {
        let mut args = vec![
            "report".to_owned(),
            self.input.to_str().ok_or("input UTF8")?.to_owned(),
            "--json-output".to_owned(),
            self.directory
                .path()
                .join("report.json")
                .to_str()
                .ok_or("report UTF8")?
                .to_owned(),
            "--markdown-output".to_owned(),
            self.directory
                .path()
                .join("report.md")
                .to_str()
                .ok_or("markdown UTF8")?
                .to_owned(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        calibration::run(&args)
    }
    fn report(&self) -> ToolResult<Value> {
        report::build_report(&self.load()?)
    }
    fn markdown(&self) -> ToolResult<String> {
        Ok(fs::read_to_string(self.directory.path().join("report.md"))?)
    }
}

fn rows<'a>(
    report: &'a Value,
    role: &str,
    variant: &str,
    metric: &str,
) -> ToolResult<Vec<&'a Value>> {
    Ok(report["comparisons"]
        .as_array()
        .ok_or("comparisons missing")?
        .iter()
        .filter(|row| row["role"] == role && row["variant"] == variant && row["metric"] == metric)
        .collect())
}

#[test]
fn discovery_only_learning_retains_holdout_noise_and_ignores_controls() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    counts(&mut fixture.training, "b", "I1mr", 110, None)?;
    counts(&mut fixture.validation, "b", "I1mr", 130, None)?;
    for job in [&mut fixture.training, &mut fixture.validation] {
        for variant in ["layout", "extra_work", "extra_alloc"] {
            counts(job, variant, "I1mr", 10000, None)?;
        }
    }
    let report = fixture.report()?;
    assert!(
        report["cache_allowances"]
            .as_array()
            .ok_or("allowances missing")?
            .iter()
            .filter(|row| row["metric"] == "I1mr")
            .all(|row| row["discovery_positive_delta_floor"] == 10)
    );
    let held = rows(&report, "validation", "b", "I1mr")?;
    assert_eq!(held.len(), 4);
    assert!(held.iter().all(|row| row["absolute_delta"] == 30
        && row["proposed_allowance"] == 10
        && row["proposed_passed"] == false));
    assert_eq!(
        report["held_out_aa"]["by_metric"]["I1mr"]["current_false_positives"],
        4
    );
    assert_eq!(
        report["held_out_aa"]["by_metric"]["I1mr"]["proposed_false_positives"],
        4
    );
    fixture.cli(&[])?;
    let markdown = fixture.markdown()?;
    for needle in [
        "Insufficient predeclared coverage",
        "dependent",
        "| I1mr | 4 | 4 | 4 |",
    ] {
        assert!(markdown.contains(needle));
    }
    assert_eq!(report["active_gates_changed"], false);
    assert_eq!(report["mode"], "proposal-only");
    Ok(())
}

#[test]
fn learned_floor_is_inclusive_and_active_allowance_can_be_larger() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    counts(&mut fixture.training, "b", "ILmr", 110, None)?;
    counts(&mut fixture.validation, "b", "ILmr", 110, None)?;
    let report = fixture.report()?;
    assert!(
        rows(&report, "validation", "b", "ILmr")?
            .iter()
            .all(|row| row["proposed_passed"] == true && row["current_passed"] == false)
    );
    counts(&mut fixture.validation, "a", "ILmr", 1000, None)?;
    counts(&mut fixture.validation, "b", "ILmr", 1030, None)?;
    let report = fixture.report()?;
    assert!(
        rows(&report, "validation", "b", "ILmr")?
            .iter()
            .all(|row| row["proposed_allowance"] == 30 && row["proposed_passed"] == true)
    );
    Ok(())
}

#[test]
fn training_ir_exceedances_are_report_only_and_never_relax_ir() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    counts(&mut fixture.training, "b", "Ir", 150, None)?;
    counts(&mut fixture.validation, "b", "Ir", 103, None)?;
    let report = fixture.report()?;
    assert!(
        rows(&report, "validation", "b", "Ir")?
            .iter()
            .all(|row| row["active_allowance"] == 2
                && row["proposed_allowance"] == 2
                && row["proposed_passed"] == false)
    );
    fixture.cli(&[])
}

#[test]
fn positive_controls_must_exceed_two_percent_for_every_validation_signal() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    counts(&mut fixture.validation, "extra_alloc", "Ir", 102, None)?;
    fixture.load()?;
    assert!(fixture.cli(&[]).is_err());
    let report = strict_json(&fs::read_to_string(
        fixture.directory.path().join("report.json"),
    )?)?;
    let failures = report["sensitivity"]["validation_failures"]
        .as_array()
        .ok_or("failures missing")?;
    let observed: BTreeSet<_> = failures
        .iter()
        .map(|row| (row["variant"].as_str(), row["id"].as_str()))
        .collect();
    assert_eq!(
        observed,
        BTreeSet::from([
            (Some("extra_alloc"), Some("small")),
            (Some("extra_alloc"), Some("medium")),
            (Some("extra_alloc"), Some("large"))
        ])
    );
    assert!(fixture.markdown()?.contains("FAIL"));
    counts(&mut fixture.validation, "extra_alloc", "Ir", 103, None)?;
    fixture.load()?;
    fixture.cli(&[])
}

#[test]
fn discovery_control_failure_and_non_inlay_control_shifts_are_diagnostic() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    counts(&mut fixture.training, "extra_work", "Ir", 100, None)?;
    for sample in fixture.validation["samples"]
        .as_array_mut()
        .ok_or("samples missing")?
    {
        if sample["variant"] == "extra_work" || sample["variant"] == "extra_alloc" {
            sample["workloads"][3]["counts"]["Ir"] = 0.into();
        }
    }
    assert_eq!(fixture.report()?["sensitivity"]["passed"], true);
    fixture.cli(&[])
}

#[test]
fn zero_baseline_is_undefined_relative_but_counts_and_policy_still_apply() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    for job in [&mut fixture.training, &mut fixture.validation] {
        for variant in VARIANTS {
            counts(job, variant, "I1mr", 0, None)?;
        }
    }
    counts(&mut fixture.validation, "b", "I1mr", 4, None)?;
    let report = fixture.report()?;
    assert!(
        rows(&report, "validation", "b", "I1mr")?
            .iter()
            .all(|row| row["absolute_delta"] == 4
                && row["relative_delta"].is_null()
                && row["relative_delta_exact"].is_null()
                && row["current_passed"] == false)
    );
    assert!(
        report["summaries"]
            .as_array()
            .ok_or("summaries missing")?
            .iter()
            .filter(|row| row["role"] == "validation"
                && row["variant"] == "b"
                && row["metric"] == "I1mr")
            .all(|row| row["relative_delta"]["undefined"] == 1)
    );
    Ok(())
}

#[test]
fn signed_distributions_nearest_rank_and_exact_relative_ratios() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    fixture.training = dataset("training", "discovery", 5);
    for (pair, count) in [97, 98, 100, 104, 110].into_iter().enumerate() {
        counts(
            &mut fixture.training,
            "b",
            "ILmr",
            count,
            Some(u64::try_from(pair + 1)?),
        )?;
    }
    let report = fixture.report()?;
    let summary = report["summaries"]
        .as_array()
        .ok_or("summaries missing")?
        .iter()
        .find(|row| {
            row["role"] == "discovery"
                && row["variant"] == "b"
                && row["metric"] == "ILmr"
                && row["id"] == "small"
        })
        .ok_or("summary missing")?;
    assert_eq!(
        summary["absolute_delta"],
        json!({"n": 5, "undefined": 0, "min": -3, "p50": 0, "p95": 10, "max": 10})
    );
    assert_eq!(
        summary["relative_delta_exact"]["min"],
        json!({"numerator": -3, "denominator": 100})
    );
    assert_eq!(
        summary["relative_delta_exact"]["max"],
        json!({"numerator": 1, "denominator": 10})
    );
    Ok(())
}

#[test]
fn binary_addresses_host_identity_and_execution_order_are_preserved() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    fixture.validation["variants"]["a"]["binary_sha256"] = json!("9".repeat(64));
    fixture.validation["variants"]["a"]["layout_probe"]["address"] = json!("9000");
    for sample in fixture.validation["samples"]
        .as_array_mut()
        .ok_or("samples missing")?
    {
        sample["order"] = json!(4 - sample["order"].as_u64().ok_or("order missing")?);
    }
    let report = fixture.report()?;
    let job = report["jobs"]
        .as_array()
        .ok_or("jobs missing")?
        .iter()
        .find(|job| job["job_id"] == "holdout")
        .ok_or("holdout missing")?;
    assert_eq!(job["variants"]["a"]["binary_sha256"], "9".repeat(64));
    assert_eq!(job["variants"]["a"]["layout_probe"]["address"], "9000");
    assert_eq!(
        job["execution_order"]
            .as_array()
            .ok_or("order missing")?
            .iter()
            .map(|sample| sample["order"].clone())
            .collect::<Vec<_>>(),
        vec![json!(4), json!(3), json!(2), json!(1), json!(0)]
    );
    assert_eq!(report["hosts"]["holdout"]["runner_identity"], "holdout");
    Ok(())
}

#[test]
fn incomparable_source_fixture_harness_environment_or_workloads_fail() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    let original = fixture.validation.clone();
    for pointer in [
        "/source_revision",
        "/fixture_sha256",
        "/harness_sha256",
        "/environment/rustc",
        "/environment/cache_args",
        "/variants/layout/source_sha256",
    ] {
        fixture.validation = original.clone();
        *fixture
            .validation
            .pointer_mut(pointer)
            .ok_or("mutation target missing")? = if pointer.ends_with("cache_args") {
            json!(["--cache-sim=no"])
        } else if pointer.ends_with("sha256") {
            json!("0".repeat(64))
        } else {
            json!("other")
        };
        assert_error(fixture.load(), "incomparable")?;
    }
    fixture.validation = original;
    for sample in fixture.validation["samples"]
        .as_array_mut()
        .ok_or("samples missing")?
    {
        sample["workloads"][3]["function_name"] = json!("unexpected_workload");
    }
    assert_error(fixture.load(), "incomparable workload")
}

#[test]
fn malformed_counts_sources_samples_and_identities_are_rejected() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    for (pointer, value) in [
        ("/format_version", json!(true)),
        ("/variants/b/source_sha256", json!("b".repeat(64))),
        ("/samples/0/pair", json!(true)),
        ("/samples/0/order", json!(true)),
        ("/samples/0/order", json!(1)),
        ("/samples/0/workloads/0/counts/Ir", json!(-1)),
        ("/samples/0/workloads/0/counts/I1mr", json!(true)),
        ("/samples/0/workloads/0/counts/ILmr", json!(1.5)),
        ("/variants/layout/layout_probe/size", json!(1023)),
        ("/variants/a/layout_probe/symbol", json!("missing_symbol")),
        ("/variants/a/layout_probe/address", json!("0")),
        ("/variants/layout/layout_probe_collected", json!(true)),
    ] {
        let mut invalid = fixture.training.clone();
        *invalid
            .pointer_mut(pointer)
            .ok_or("mutation target missing")? = value;
        // Invalid pair types cannot synthesize retained evidence; rewrite the
        // dataset after evidence creation so the reader exercises the contract.
        write_job(&fixture.input.join("0/nested"), &fixture.training)?;
        write_json(&fixture.input.join("0/nested/data.json"), &invalid)?;
        write_job(&fixture.input.join("1/nested"), &fixture.validation)?;
        assert!(report::read_datasets(&fixture.input).is_err(), "{pointer}");
    }
    for mutation in [
        "duplicate_sample",
        "missing_sample",
        "missing_workload",
        "duplicate_workload",
        "missing_metric",
        "extra_metric",
    ] {
        let mut invalid = fixture.training.clone();
        match mutation {
            "duplicate_sample" => {
                let sample = invalid["samples"][0].clone();
                invalid["samples"]
                    .as_array_mut()
                    .ok_or("samples missing")?
                    .push(sample);
            }
            "missing_sample" => {
                invalid["samples"]
                    .as_array_mut()
                    .ok_or("samples missing")?
                    .pop();
            }
            "missing_workload" => {
                invalid["samples"][0]["workloads"]
                    .as_array_mut()
                    .ok_or("workloads missing")?
                    .pop();
            }
            "duplicate_workload" => {
                let workload = invalid["samples"][0]["workloads"][0].clone();
                invalid["samples"][0]["workloads"]
                    .as_array_mut()
                    .ok_or("workloads missing")?
                    .push(workload);
            }
            "missing_metric" => {
                invalid["samples"][0]["workloads"][0]["counts"]
                    .as_object_mut()
                    .ok_or("counts missing")?
                    .remove("Ir");
            }
            _ => invalid["samples"][0]["workloads"][0]["counts"]["Dr"] = json!(10),
        }
        assert!(
            fixture
                .load_jobs(&[invalid, fixture.validation.clone()])
                .is_err(),
            "{mutation}"
        );
    }
    Ok(())
}

#[test]
fn duplicate_jobs_and_missing_split_are_rejected() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    fixture.validation["job_id"] = json!("training");
    assert_error(fixture.load(), "duplicate job_id")?;
    fixture.validation["job_id"] = json!("holdout");
    fixture.validation["role"] = json!("discovery");
    assert_error(fixture.load(), "held-out validation")
}

#[test]
fn recursive_reader_ignores_raw_json_but_rejects_duplicate_json_keys() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    fixture.load()?;
    fs::write(fixture.input.join("irrelevant.json"), "not JSON")?;
    assert_eq!(report::read_datasets(&fixture.input)?.len(), 2);
    let path = fixture.input.join("0/nested/data.json");
    let text = fs::read_to_string(&path)?;
    fs::write(
        path,
        text.replace(
            "\"format_version\": 1",
            "\"format_version\": 1, \"format_version\": 1",
        ),
    )?;
    assert_error(fixture.cli(&[]), "duplicate JSON object key")?;
    assert!(!fixture.directory.path().join("report.json").exists());
    Ok(())
}

#[test]
fn missing_and_escaping_raw_evidence_is_rejected() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    fixture.load()?;
    let root = fixture.input.join("0/nested");
    let path = root.join(
        fixture.training["samples"][0]["raw_stdout"]
            .as_str()
            .ok_or("stdout missing")?,
    );
    fs::remove_file(&path)?;
    assert_error(report::read_datasets(&fixture.input), "evidence file")?;
    #[cfg(unix)]
    {
        let outside = fixture.directory.path().join("outside.log");
        fs::write(&outside, "outside")?;
        std::os::unix::fs::symlink(outside, path)?;
        assert_error(report::read_datasets(&fixture.input), "evidence file")?;
    }
    fixture.training["samples"][0]["raw_stdout"] = json!("../outside.log");
    write_json(&root.join("data.json"), &fixture.training)?;
    assert_error(report::read_datasets(&fixture.input), "must not escape")
}

#[test]
fn retained_stdout_must_match_counts_execution_and_fresh_baseline() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    for corruption in [
        "invalid_json",
        "count",
        "executable",
        "baseline",
        "duplicate",
        "missing",
        "dataset_count",
    ] {
        fixture.load()?;
        let root = fixture.input.join("0/nested");
        let path = root.join(
            fixture.training["samples"][0]["raw_stdout"]
                .as_str()
                .ok_or("stdout missing")?,
        );
        if corruption == "invalid_json" {
            fs::write(&path, "not JSON\n")?;
        } else if corruption == "dataset_count" {
            let mut data = fixture.training.clone();
            data["samples"][0]["workloads"][0]["counts"]["Ir"] = json!(101);
            write_json(&root.join("data.json"), &data)?;
        } else {
            let mut rows = fs::read_to_string(&path)?
                .lines()
                .map(strict_json)
                .collect::<ToolResult<Vec<_>>>()?;
            match corruption {
                "count" => {
                    rows[0]["profiles"][0]["summaries"]["parts"][0]["metrics_summary"]["Callgrind"]
                        ["Ir"]["metrics"]["Left"]["Int"] = json!(101);
                }
                "executable" => rows[0]["benchmark_exe"] = json!("/stale/analysis"),
                "baseline" => rows[0]["baselines"] = json!(["stale", "stale"]),
                "duplicate" => rows.push(rows[0].clone()),
                _ => {
                    rows.pop();
                }
            }
            let text = rows
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()?
                .join("\n");
            fs::write(&path, text)?;
        }
        assert!(
            report::read_datasets(&fixture.input).is_err(),
            "{corruption}"
        );
    }
    Ok(())
}

#[test]
fn optional_expected_coverage_fails_missing_matrix_jobs_or_pairs() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    fixture.load()?;
    fixture.cli(&[
        "--expected-discovery-jobs",
        "1",
        "--expected-validation-jobs",
        "1",
        "--expected-pairs",
        "1",
    ])?;
    for options in [
        ["--expected-discovery-jobs", "7"],
        ["--expected-validation-jobs", "3"],
        ["--expected-pairs", "5"],
        ["--expected-pairs", "0"],
    ] {
        assert!(fixture.cli(&options).is_err());
    }
    assert!(
        Arguments::try_parse_from([
            "report",
            "input",
            "--json-output",
            "a",
            "--markdown-output",
            "b",
            "--expected-pairs",
            "0"
        ])
        .is_err()
    );
    Ok(())
}

#[test]
fn missing_required_signals_and_noncontiguous_pairs_fail_closed() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    for sample in fixture.training["samples"]
        .as_array_mut()
        .ok_or("samples missing")?
    {
        sample["workloads"]
            .as_array_mut()
            .ok_or("workloads missing")?
            .retain(|workload| workload["id"] != "medium");
    }
    assert_error(fixture.load(), "missing small/medium/large")?;
    fixture.training = dataset("training", "discovery", 1);
    for sample in fixture.training["samples"]
        .as_array_mut()
        .ok_or("samples missing")?
    {
        sample["pair"] = json!(2);
    }
    assert_error(fixture.load(), "contiguous")
}

#[test]
fn zero_ir_positive_controls_require_a_real_increase() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    for job in [&mut fixture.training, &mut fixture.validation] {
        for variant in VARIANTS {
            counts(job, variant, "Ir", 0, None)?;
        }
        for variant in ["extra_work", "extra_alloc"] {
            counts(job, variant, "Ir", 1, None)?;
        }
    }
    assert_eq!(fixture.report()?["sensitivity"]["passed"], true);
    counts(&mut fixture.validation, "extra_work", "Ir", 0, None)?;
    let report = fixture.report()?;
    assert_eq!(report["sensitivity"]["passed"], false);
    assert_eq!(
        report["sensitivity"]["validation_failures"]
            .as_array()
            .ok_or("failures missing")?
            .len(),
        3
    );
    Ok(())
}

#[test]
fn predeclared_coverage_requires_full_seven_three_five_design() -> ToolResult<()> {
    let fixture = Fixture::new()?;
    let jobs: Vec<_> = [("discovery", 7), ("validation", 3)]
        .into_iter()
        .flat_map(|(role, count)| {
            (0..count).map(move |index| dataset(&format!("{role}-{index}"), role, 5))
        })
        .collect();
    let report = report::build_report(&fixture.load_jobs(&jobs)?)?;
    assert_eq!(report["coverage"]["predeclared_coverage_met"], true);
    assert!(
        report["coverage"]["limitation"]
            .as_str()
            .ok_or("limitation missing")?
            .contains("do not establish tail reliability")
    );
    Ok(())
}

#[test]
fn signed_u64_extremes_retain_exact_delta_and_ratio_without_overflow() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    for job in [&mut fixture.training, &mut fixture.validation] {
        counts(job, "a", "ILmr", u64::MAX, None)?;
        counts(job, "b", "ILmr", 0, None)?;
    }
    let report = fixture.report()?;
    let held = rows(&report, "validation", "b", "ILmr")?;
    assert_eq!(
        held[0]["absolute_delta"].to_string(),
        "-18446744073709551615"
    );
    assert_eq!(
        held[0]["relative_delta_exact"]["denominator"],
        json!(u64::MAX)
    );
    fixture.cli(&[])
}

#[test]
fn environment_rejects_host_metadata_and_allows_comparable_cache_geometry() -> ToolResult<()> {
    let mut fixture = Fixture::new()?;
    fixture.training["environment"]["runner_identity"] = json!("host");
    assert_error(fixture.load(), "put host metadata in host")?;
    fixture.training["environment"]
        .as_object_mut()
        .ok_or("environment missing")?
        .remove("runner_identity");
    for job in [&mut fixture.training, &mut fixture.validation] {
        job["environment"]["cache_geometry"] = json!({"L1": 32768});
    }
    fixture.load()?;
    assert_eq!(METRICS, ["Ir", "I1mr", "ILmr"]);
    Ok(())
}
