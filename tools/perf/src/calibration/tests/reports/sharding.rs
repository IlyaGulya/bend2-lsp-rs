//! Shard aggregation is user-visible: incomplete, overlapping, shuffled or
//! mixed-run physical datasets must never be reported as a complete series.
use super::multibench::{dataset_v2, write_v2};
use crate::{
    ToolResult,
    calibration::{Arguments, Command, VARIANTS, report, shard},
    callgrind::{BENCHMARKS, Identity, identity},
    common::write_json,
};
use clap::Parser as _;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

const RUNNER_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OTHER_RUNNER_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const EXTRA_ANALYSIS: [(&str, &[Option<&str>]); 2] = [
    ("shard_alpha", &[Some("one"), Some("two")]),
    ("shard_beta", &[None]),
];

fn full_selection(job: &Value) -> ToolResult<shard::Selection> {
    BENCHMARKS
        .into_iter()
        .map(|benchmark| {
            let rows = job["variants"]["a"]["benchmarks"][benchmark]["workloads"]
                .as_array()
                .ok_or("frozen workloads missing")?;
            let identities = rows
                .iter()
                .map(identity)
                .collect::<ToolResult<BTreeSet<_>>>()?;
            Ok((benchmark, identities))
        })
        .collect()
}

/// Give the fixture four distinct analysis function units so every physical
/// shard owns at least one whole unit; all cases of each unit stay together.
fn extended_job(logical: &str, role: &str) -> ToolResult<Value> {
    let mut job = dataset_v2(logical, role)?;
    for variant in VARIANTS {
        let rows = job["variants"][variant]["benchmarks"]["analysis"]["workloads"]
            .as_array_mut()
            .ok_or("analysis workloads missing")?;
        for (name, ids) in EXTRA_ANALYSIS {
            for id in ids {
                rows.push(json!({"function_name": name, "id": id}));
            }
        }
    }
    for sample in job["samples"].as_array_mut().ok_or("samples missing")? {
        let controls = matches!(
            sample["variant"].as_str(),
            Some("extra_work" | "extra_alloc")
        );
        let workloads = sample["workloads"]
            .as_array_mut()
            .ok_or("workloads missing")?;
        for (name, ids) in EXTRA_ANALYSIS {
            for id in ids {
                workloads.push(json!({
                    "function_name": name,
                    "id": id,
                    "counts": {"Ir": if controls { 110 } else { 100 }, "I1mr": 100, "ILmr": 100}
                }));
            }
        }
    }
    Ok(job)
}

fn shard_dataset(
    logical: &str,
    role: &str,
    index: u64,
    count: u64,
    runner: &str,
    attempt: &str,
) -> ToolResult<Value> {
    let mut job = extended_job(logical, role)?;
    let full = full_selection(&job)?;
    let selected = shard::selection(&full, count, index)?;
    let physical = if count == 1 {
        logical.to_owned()
    } else {
        format!("{logical}-shard-{index}")
    };
    job["host"] = json!({"runner_identity": runner, "hostname": format!("host-{physical}"), "kernel": "test"});
    job["shard"] = json!({
        "method": shard::METHOD,
        "count": count,
        "index": index,
        "logical_job_id": logical,
        "physical_job_id": physical,
        "matrix_job": physical,
        "run_id": "100",
        "run_attempt": attempt,
        "physical_runner": runner,
        "full_manifest": shard::manifest(&full)?,
        "selected_units": shard::units(&selected),
    });
    for sample in job["samples"].as_array_mut().ok_or("samples missing")? {
        let workloads = sample["workloads"]
            .as_array()
            .ok_or("workloads missing")?
            .clone();
        let mut kept = Vec::new();
        for workload in workloads {
            let name = workload["function_name"]
                .as_str()
                .ok_or("function name missing")?;
            let benchmark = if name == "reference_location_order_warm" {
                "reference_locations"
            } else {
                "analysis"
            };
            if selected[benchmark].contains(&identity(&workload)?) {
                kept.push(workload);
            }
        }
        sample["workloads"] = json!(kept);
        let benchmarks = sample["benchmarks"]
            .as_object_mut()
            .ok_or("benchmarks missing")?;
        benchmarks.retain(|benchmark, _| !selected[benchmark.as_str()].is_empty());
        for evidence in benchmarks.values_mut() {
            evidence["execution"] = json!({"collection": "collection.json"});
        }
    }
    Ok(job)
}

/// The producer writes one runner receipt per measured pair, not per benchmark.
fn write_shard(root: &Path, job: &Value) -> ToolResult<()> {
    write_v2(root, job)?;
    for sample in job["samples"].as_array().ok_or("samples missing")? {
        let mut pair = None;
        for benchmark in BENCHMARKS {
            let Some(evidence) = sample["benchmarks"].get(benchmark) else {
                continue;
            };
            let stdout = root.join(evidence["raw_stdout"].as_str().ok_or("stdout missing")?);
            pair = Some(
                stdout
                    .parent()
                    .ok_or("stdout parent missing")?
                    .parent()
                    .ok_or("stdout is not inside a pair directory")?
                    .to_owned(),
            );
            break;
        }
        let pair = pair.ok_or("selected sample has no benchmark evidence")?;
        write_json(
            &pair.join("collection.json"),
            &json!({
                "original_runner": "/pinned/iai-callgrind-runner",
                "original_runner_sha256": RUNNER_SHA,
                "parallelism": 2,
                "functions": sample["workloads"].as_array().map_or(0, Vec::len),
            }),
        )?;
    }
    Ok(())
}

fn write_complete(input: &Path) -> ToolResult<()> {
    for (logical, role) in [
        ("discovery-01", "discovery"),
        ("validation-01", "validation"),
    ] {
        for index in 0..shard::MAX_COUNT {
            let runner = format!("runner-{logical}-{index}");
            let job = shard_dataset(logical, role, index, shard::MAX_COUNT, &runner, "1")?;
            write_shard(&input.join(logical).join(format!("shard-{index}")), &job)?;
        }
    }
    Ok(())
}

fn dataset_path(input: &Path, logical: &str, index: u64) -> PathBuf {
    input
        .join(logical)
        .join(format!("shard-{index}"))
        .join("data.json")
}

fn rewrite(path: &Path, mutate: impl FnOnce(&mut Value) -> ToolResult<()>) -> ToolResult<()> {
    let mut job: Value = serde_json::from_slice(&fs::read(path)?)?;
    mutate(&mut job)?;
    write_json(path, &job)
}

fn copy_tree(source: &Path, destination: &Path) -> ToolResult<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn rewrite_receipts(input: &Path, logical: &str, sha: &str, shards: &[u64]) -> ToolResult<()> {
    for index in shards {
        for variant in VARIANTS {
            let path = input.join(format!(
                "{logical}/shard-{index}/raw/1-{variant}/collection.json"
            ));
            let mut receipt: Value = serde_json::from_slice(&fs::read(&path)?)?;
            receipt["original_runner_sha256"] = json!(sha);
            write_json(&path, &receipt)?;
        }
    }
    Ok(())
}

fn assert_rejected(input: &Path, message: &str) -> ToolResult<()> {
    let error = report::read_datasets(input).err().ok_or("expected error")?;
    assert!(error.to_string().contains(message), "{error}");
    Ok(())
}

fn unit_identities(units: &Value) -> ToolResult<Vec<Identity>> {
    let mut identities = Vec::new();
    for row in units.as_array().ok_or("selected units missing")? {
        let name = row["function_name"]
            .as_str()
            .ok_or("unit function name missing")?
            .to_owned();
        for id in row["ids"].as_array().ok_or("unit ids missing")? {
            let id = match id {
                Value::Null => None,
                Value::String(id) => Some(id.clone()),
                _ => return Err("unit id must be null or a string".into()),
            };
            identities.push((name.clone(), id));
        }
    }
    Ok(identities)
}

#[test]
fn deterministic_four_whole_function_shards_are_disjoint_and_exhaustive() -> ToolResult<()> {
    let job = extended_job("discovery-01", "discovery")?;
    let full = full_selection(&job)?;
    let mut covered: BTreeMap<&'static str, BTreeSet<Identity>> = BENCHMARKS
        .into_iter()
        .map(|benchmark| (benchmark, BTreeSet::new()))
        .collect();
    let mut seen = BTreeSet::new();
    let mut sizes = Vec::new();
    for index in 0..shard::MAX_COUNT {
        let selected = shard::selection(&full, shard::MAX_COUNT, index)?;
        assert_eq!(
            selected,
            shard::selection(&full, shard::MAX_COUNT, index)?,
            "partition must be deterministic"
        );
        sizes.push(shard::count(&selected));
        for benchmark in BENCHMARKS {
            let selected_names: BTreeSet<_> = selected[benchmark]
                .iter()
                .map(|identity| identity.0.clone())
                .collect();
            for name in &selected_names {
                let original: BTreeSet<_> = full[benchmark]
                    .iter()
                    .filter(|identity| &identity.0 == name)
                    .cloned()
                    .collect();
                let owned: BTreeSet<_> = selected[benchmark]
                    .iter()
                    .filter(|identity| &identity.0 == name)
                    .cloned()
                    .collect();
                assert_eq!(owned, original, "whole function unit must stay together");
            }
            for identity in &selected[benchmark] {
                assert!(seen.insert(identity.clone()), "shards must be disjoint");
                covered
                    .get_mut(benchmark)
                    .ok_or("benchmark missing")?
                    .insert(identity.clone());
            }
        }
    }
    assert_eq!(covered, full, "shards must be exhaustive");
    assert_eq!(sizes, [4, 3, 2, 1]);
    assert_eq!(
        shard::selection(&full, 4, 4)
            .err()
            .ok_or("index")?
            .to_string(),
        "shard index must be less than shard count"
    );
    assert!(shard::selection(&full, 5, 0).is_err());
    assert_eq!(shard::count(&shard::selection(&full, 2, 0)?), 6);
    assert_eq!(shard::count(&shard::selection(&full, 2, 1)?), 4);
    Ok(())
}

#[test]
fn canonical_manifest_dodges_newline_null_and_empty_string_collisions() -> ToolResult<()> {
    let selection = |name: &str, id: Option<&str>| -> shard::Selection {
        BTreeMap::from([
            (
                "analysis",
                BTreeSet::from([(name.to_owned(), id.map(str::to_owned))]),
            ),
            ("reference_locations", BTreeSet::new()),
        ])
    };
    let embedded = shard::manifest(&selection("f", Some("x\ny")))?;
    let moved = shard::manifest(&selection("f\nx", Some("y")))?;
    assert_ne!(embedded["sha256"], moved["sha256"]);
    assert_ne!(embedded["identities"], moved["identities"]);
    let absent = shard::manifest(&selection("f", None))?;
    let empty = shard::manifest(&selection("f", Some("")))?;
    assert_ne!(absent["sha256"], empty["sha256"]);
    assert_ne!(absent["identities"], empty["identities"]);
    let quoted = shard::manifest(&selection("f\"x", Some("y")))?;
    assert_ne!(quoted["sha256"], embedded["sha256"]);
    Ok(())
}

#[test]
fn complete_shards_recompose_one_host_series_and_full_scope() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    write_complete(directory.path())?;
    assert!(
        directory
            .path()
            .join("discovery-01/shard-0/raw/1-a/collection.json")
            .is_file(),
        "producer receipt lives in the pair directory"
    );
    assert!(
        !directory
            .path()
            .join("discovery-01/shard-0/raw/1-a/analysis/collection.json")
            .exists(),
        "producer does not write a per-benchmark receipt"
    );
    let jobs = report::read_datasets(directory.path())?;
    assert_eq!(jobs.len(), 2);
    report::check_expected_shards(&jobs, Some(4))?;
    assert!(report::check_expected_shards(&jobs, Some(2)).is_err());
    let merged = jobs
        .iter()
        .find(|job| job["job_id"] == "discovery-01")
        .ok_or("merged job missing")?;
    assert_eq!(
        merged["shards"].as_array().ok_or("shards missing")?.len(),
        4
    );
    let samples = merged["samples"].as_array().ok_or("samples missing")?;
    assert_eq!(samples.len(), 5);
    let mut origin: BTreeMap<Identity, u64> = BTreeMap::new();
    for shard in merged["shards"].as_array().ok_or("shards missing")? {
        let index = shard["index"].as_u64().ok_or("index missing")?;
        assert_eq!(
            shard["physical_job_id"],
            format!("discovery-01-shard-{index}")
        );
        for benchmark in BENCHMARKS {
            for identity in unit_identities(&shard["selected_units"][benchmark])? {
                assert!(
                    origin.insert(identity, index).is_none(),
                    "one host owns each workload series"
                );
            }
        }
    }
    let mut counts: BTreeMap<Identity, usize> = BTreeMap::new();
    for sample in samples {
        assert_eq!(
            sample["workloads"]
                .as_array()
                .ok_or("workloads missing")?
                .len(),
            10
        );
        assert_eq!(sample["shards"], json!([0, 1, 2, 3]));
        for row in sample["workloads"].as_array().ok_or("workloads missing")? {
            let id = identity(row)?;
            *counts.entry(id.clone()).or_default() += 1;
            let index = origin[&id];
            assert!(
                sample["shards"]
                    .as_array()
                    .ok_or("shards missing")?
                    .contains(&json!(index)),
                "each row must come from its single owning physical shard"
            );
        }
    }
    assert_eq!(counts.len(), 10);
    assert!(counts.values().all(|count| *count == 5));
    assert_eq!(merged["shards"][0]["run_id"], "100");
    assert_eq!(
        merged["shards"][0]["host"]["runner_identity"],
        "runner-discovery-01-0"
    );
    Ok(())
}

#[test]
fn rejects_incomplete_duplicate_shuffled_stale_or_mixed_shards() -> ToolResult<()> {
    for kind in [
        "missing",
        "duplicate-index",
        "shuffled-units",
        "unknown-unit",
        "mixed-attempt",
        "stale-source",
        "mixed-profile",
        "mixed-job-profile",
        "host-mismatch",
        "out-of-range",
    ] {
        let directory = tempfile::tempdir()?;
        write_complete(directory.path())?;
        let input = directory.path();
        match kind {
            "missing" => {
                fs::remove_dir_all(input.join("discovery-01/shard-3"))?;
            }
            "duplicate-index" => {
                fs::remove_dir_all(input.join("discovery-01/shard-1"))?;
                copy_tree(
                    &input.join("discovery-01/shard-0"),
                    &input.join("discovery-01/shard-1"),
                )?;
            }
            "shuffled-units" => {
                let other: Value =
                    serde_json::from_slice(&fs::read(dataset_path(input, "discovery-01", 1))?)?;
                rewrite(&dataset_path(input, "discovery-01", 0), |job| {
                    job["shard"]["selected_units"] = other["shard"]["selected_units"].clone();
                    Ok(())
                })?;
            }
            "unknown-unit" => rewrite(&dataset_path(input, "discovery-01", 0), |job| {
                job["shard"]["selected_units"]["analysis"]
                    .as_array_mut()
                    .ok_or("units missing")?
                    .push(json!({"function_name": "ghost_workload", "ids": [null]}));
                Ok(())
            })?,
            "mixed-attempt" => rewrite(&dataset_path(input, "discovery-01", 2), |job| {
                job["shard"]["run_attempt"] = json!("2");
                Ok(())
            })?,
            "stale-source" => rewrite(&dataset_path(input, "discovery-01", 1), |job| {
                job["source_revision"] = json!("other-revision");
                Ok(())
            })?,
            "mixed-profile" => rewrite_receipts(input, "discovery-01", OTHER_RUNNER_SHA, &[1])?,
            "mixed-job-profile" => {
                rewrite_receipts(input, "discovery-01", OTHER_RUNNER_SHA, &[0, 1, 2, 3])?;
            }
            "host-mismatch" => rewrite(&dataset_path(input, "discovery-01", 0), |job| {
                job["host"]["runner_identity"] = json!("foreign-runner");
                Ok(())
            })?,
            _ => rewrite(&dataset_path(input, "discovery-01", 3), |job| {
                job["shard"]["index"] = json!(4);
                Ok(())
            })?,
        }
        let expected = match kind {
            "missing" | "duplicate-index" => "physical shard",
            "shuffled-units" | "unknown-unit" => "selected units do not match",
            "mixed-attempt" => "mixed workflow run or attempt",
            "stale-source" => "incomparable source_revision",
            "mixed-profile" => "across physical shards",
            "mixed-job-profile" => "across jobs",
            "host-mismatch" => "physical runner identity must match",
            _ => "shard index must be less",
        };
        assert_rejected(input, expected)?;
    }
    Ok(())
}

fn comparison_map(report: &Value) -> ToolResult<BTreeMap<String, Value>> {
    let mut rows = BTreeMap::new();
    for row in report["comparisons"]
        .as_array()
        .ok_or("comparisons missing")?
    {
        rows.insert(
            format!(
                "{}/{}/{}/{}/{}/{}/{}",
                row["job_id"],
                row["role"],
                row["pair"],
                row["variant"],
                row["function_name"],
                row["id"],
                row["metric"]
            ),
            row.clone(),
        );
    }
    Ok(rows)
}

#[test]
fn sharded_aggregation_preserves_unsharded_comparisons_and_sensitivity() -> ToolResult<()> {
    let unsharded = tempfile::tempdir()?;
    let sharded = tempfile::tempdir()?;
    for (logical, role) in [
        ("discovery-01", "discovery"),
        ("validation-01", "validation"),
    ] {
        let job = extended_job(logical, role)?;
        write_v2(&unsharded.path().join(logical), &job)?;
        for index in 0..shard::MAX_COUNT {
            let runner = format!("runner-{logical}-{index}");
            let shard_job = shard_dataset(logical, role, index, shard::MAX_COUNT, &runner, "1")?;
            write_shard(
                &sharded.path().join(logical).join(format!("shard-{index}")),
                &shard_job,
            )?;
        }
    }
    let unsharded_report = report::build_report(&report::read_datasets(unsharded.path())?)?;
    let sharded_report = report::build_report(&report::read_datasets(sharded.path())?)?;
    assert_eq!(
        comparison_map(&unsharded_report)?,
        comparison_map(&sharded_report)?,
        "aggregation must not alter raw pair comparisons"
    );
    assert_eq!(unsharded_report["summaries"], sharded_report["summaries"]);
    assert_eq!(
        unsharded_report["sensitivity"]["passed"],
        sharded_report["sensitivity"]["passed"]
    );
    assert_eq!(sharded_report["sensitivity"]["passed"], true);
    let controls: Vec<_> = sharded_report["comparisons"]
        .as_array()
        .ok_or("comparisons missing")?
        .iter()
        .filter(|row| {
            row["role"] == "validation"
                && matches!(row["variant"].as_str(), Some("extra_work" | "extra_alloc"))
                && row["function_name"] == "inlay_hints_warm"
                && matches!(row["id"].as_str(), Some("small" | "medium" | "large"))
                && row["metric"] == "Ir"
        })
        .collect();
    assert_eq!(controls.len(), 6);
    assert!(
        controls
            .iter()
            .all(|row| row["absolute_delta"] == 10 && row["current_passed"] == false),
        "shard aggregation must preserve unchanged-Ir validation control sensitivity"
    );
    assert_eq!(sharded_report["control_validation"]["passed"], false);
    assert_eq!(sharded_report["active_gates_changed"], false);
    Ok(())
}

#[test]
fn cli_accepts_bounded_shards_and_rejects_out_of_range_arguments() -> ToolResult<()> {
    let base = [
        "collect",
        "--source",
        "source",
        "--work-dir",
        "work",
        "--output-dir",
        "output",
        "--job-id",
        "job",
        "--role",
        "discovery",
    ];
    let Command::Collect(parsed) = Arguments::try_parse_from(base)?.command else {
        return Err("wrong command".into());
    };
    assert_eq!((parsed.shard_index, parsed.shard_count), (0, 1));
    let Command::Collect(parsed) = Arguments::try_parse_from(base.into_iter().chain([
        "--shard-index",
        "3",
        "--shard-count",
        "4",
    ]))?
    .command
    else {
        return Err("wrong command".into());
    };
    assert_eq!((parsed.shard_index, parsed.shard_count), (3, 4));
    for extra in [
        ["--shard-count", "0"],
        ["--shard-count", "5"],
        ["--shard-index", "-1"],
    ] {
        assert!(Arguments::try_parse_from(base.into_iter().chain(extra)).is_err());
    }
    let report_base = [
        "report",
        "input",
        "--json-output",
        "a",
        "--markdown-output",
        "b",
    ];
    let Command::Report(parsed) =
        Arguments::try_parse_from(report_base.into_iter().chain(["--expected-shards", "4"]))?
            .command
    else {
        return Err("wrong command".into());
    };
    assert_eq!(parsed.expected_shards, Some(4));
    assert!(
        Arguments::try_parse_from(report_base.into_iter().chain(["--expected-shards", "0"]))
            .is_err()
    );
    Ok(())
}
