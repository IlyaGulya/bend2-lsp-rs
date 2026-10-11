use serde_json::{Value, json};
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;
type Mutation = (&'static str, fn(&mut Value));
const WORKLOADS: [&str; 7] = [
    "hover_warm",
    "definition_warm",
    "completion_warm",
    "open_to_hover_large",
    "edit_to_hover_large",
    "hover_during_large_edit",
    "hover_during_large_open",
];

fn measurement(rounds: &[Vec<u64>], binary_hash: char) -> Value {
    let workloads: serde_json::Map<String, Value> = WORKLOADS
        .iter()
        .map(|name| ((*name).into(), json!({"rounds_ns": rounds})))
        .collect();
    json!({
        "format_version": 1,
        "workload_digest": "d".repeat(64),
        "metadata": {
            "binary_sha256": binary_hash.to_string().repeat(64),
            "platform": "test-platform", "machine": "test-machine",
            "harness": "bend2-perf/test", "rounds": rounds.len(),
            "samples": rounds.first().map_or(0, Vec::len), "warmup": 0,
        },
        "workloads": workloads,
    })
}

struct Fixture {
    directory: TempDir,
    baseline: PathBuf,
    candidate: PathBuf,
    report: PathBuf,
    markdown: PathBuf,
}

impl Fixture {
    fn new(baseline: &Value, candidate: &Value) -> Result<Self, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let baseline_path = directory.path().join("baseline.json");
        let candidate_path = directory.path().join("candidate.json");
        fs::write(&baseline_path, serde_json::to_vec(baseline)?)?;
        fs::write(&candidate_path, serde_json::to_vec(candidate)?)?;
        Ok(Self {
            report: directory.path().join("report.json"),
            markdown: directory.path().join("report.md"),
            baseline: baseline_path,
            candidate: candidate_path,
            directory,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-perf"));
        command
            .args(["reports", "latency"])
            .arg(&self.baseline)
            .arg(&self.candidate)
            .arg("--json-output")
            .arg(&self.report)
            .arg("--markdown-output")
            .arg(&self.markdown)
            .env_remove("GITHUB_ACTIONS");
        command
    }

    fn run_success(&self) -> Result<Value, Box<dyn Error>> {
        let output = self.command().output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice(&fs::read(&self.report)?)?)
    }

    fn assert_failure(&self, output: &Output, case: &str) {
        assert!(!output.status.success(), "{case} unexpectedly succeeded");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .to_lowercase()
                .contains("error:"),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!self.report.exists(), "{case} wrote a JSON report");
        assert!(!self.markdown.exists(), "{case} wrote a Markdown report");
    }
}

#[test]
fn report_uses_nearest_rank_and_median_of_round_percentiles() -> TestResult {
    let rounds = vec![
        vec![1_000_000, 2_000_000, 3_000_000, 100_000_000],
        vec![10_000_000, 20_000_000, 30_000_000, 40_000_000],
        vec![4_000_000, 5_000_000, 6_000_000, 7_000_000],
        vec![50_000_000, 60_000_000, 70_000_000, 80_000_000],
    ];
    let candidate: Vec<Vec<u64>> = rounds
        .iter()
        .map(|round| round.iter().map(|value| value * 2).collect())
        .collect();
    let fixture = Fixture::new(&measurement(&rounds, 'a'), &measurement(&candidate, 'b'))?;
    let report = fixture.run_success()?;
    let workload = &report["workloads"]["hover_warm"];
    assert_eq!(
        workload["baseline"],
        json!({
            "rounds": [
                {"p50_ns": 2_000_000, "p95_ns": 100_000_000},
                {"p50_ns": 20_000_000, "p95_ns": 40_000_000},
                {"p50_ns": 5_000_000, "p95_ns": 7_000_000},
                {"p50_ns": 60_000_000, "p95_ns": 80_000_000},
            ], "p50_ns": 12_500_000, "p95_ns": 60_000_000,
        })
    );
    assert_eq!(workload["candidate"]["p50_ns"], json!(25_000_000));
    assert_eq!(workload["candidate"]["p95_ns"], json!(120_000_000));
    assert_eq!(
        workload["delta"],
        json!({
            "p50_ns": 12_500_000, "p95_ns": 60_000_000,
            "p50_percent": 100.0, "p95_percent": 100.0,
            "rounds": [
                {"p50_ns": 2_000_000, "p95_ns": 100_000_000, "p50_percent": 100.0, "p95_percent": 100.0},
                {"p50_ns": 20_000_000, "p95_ns": 40_000_000, "p50_percent": 100.0, "p95_percent": 100.0},
                {"p50_ns": 5_000_000, "p95_ns": 7_000_000, "p50_percent": 100.0, "p95_percent": 100.0},
                {"p50_ns": 60_000_000, "p95_ns": 80_000_000, "p50_percent": 100.0, "p95_percent": 100.0},
            ],
        })
    );
    assert_eq!(report["format_version"], json!(1));
    assert_eq!(report["mode"], json!("report-only"));
    assert_eq!(report["workload_digest"], json!("d".repeat(64)));
    let markdown = fs::read_to_string(&fixture.markdown)?;
    assert!(
        markdown
            .contains("| hover_warm | 12.500 | 25.000 | +100.00% | 60.000 | 120.000 | +100.00% |")
    );
    assert!(markdown.to_lowercase().contains("report-only"));
    for name in WORKLOADS {
        assert!(markdown.contains(&format!("| {name} |")));
        assert_eq!(report["workloads"][name], *workload);
    }
    Ok(())
}

#[test]
fn improvement_and_even_round_half_nanosecond_median() -> TestResult {
    let fixture = Fixture::new(
        &measurement(&[vec![3], vec![4]], 'a'),
        &measurement(&[vec![1], vec![2]], 'b'),
    )?;
    let report = fixture.run_success()?;
    let workload = &report["workloads"]["hover_warm"];
    assert_eq!(workload["baseline"]["p50_ns"], json!(3.5));
    assert_eq!(workload["candidate"]["p95_ns"], json!(1.5));
    assert_eq!(workload["delta"]["p50_ns"], json!(-2));
    let percent = workload["delta"]["p95_percent"]
        .as_f64()
        .ok_or("missing percent")?;
    assert!((percent - (-57.142_857_142_857_14)).abs() < 1e-12);
    assert!(fs::read_to_string(&fixture.markdown)?.contains("-57.14%"));
    Ok(())
}

#[test]
fn descriptive_metadata_can_differ_without_affecting_pairing() -> TestResult {
    let mut baseline = measurement(&[vec![2, 4]], 'a');
    let mut candidate = measurement(&[vec![2, 4]], 'b');
    baseline["metadata"]["variant_order_by_round"] = json!([0]);
    candidate["metadata"]["variant_order_by_round"] = json!([1]);
    baseline["metadata"]["native_provenance"] =
        json!({"path": "base/native.json", "sha256": "a".repeat(64)});
    candidate["metadata"]["native_provenance"] =
        json!({"path": "candidate/native.json", "sha256": "b".repeat(64)});
    let fixture = Fixture::new(&baseline, &candidate)?;
    let report = fixture.run_success()?;
    assert_eq!(
        report["workloads"]["hover_warm"]["delta"]["p95_percent"],
        json!(0.0)
    );
    assert_eq!(report["baseline_metadata"], baseline["metadata"]);
    assert_eq!(report["candidate_metadata"], candidate["metadata"]);
    Ok(())
}

#[test]
fn individually_valid_but_unpaired_dimensions_fail() -> TestResult {
    for candidate in [vec![vec![1, 2], vec![3, 4]], vec![vec![1, 2, 3]]] {
        let fixture = Fixture::new(
            &measurement(&[vec![1, 2]], 'a'),
            &measurement(&candidate, 'b'),
        )?;
        fixture.assert_failure(&fixture.command().output()?, "unpaired dimensions");
    }
    Ok(())
}

#[test]
fn hundredfold_regression_is_reported_without_failure() -> TestResult {
    let fixture = Fixture::new(
        &measurement(&[vec![1, 2, 3]], 'a'),
        &measurement(&[vec![100, 200, 300]], 'a'),
    )?;
    let report = fixture.run_success()?;
    for name in WORKLOADS {
        assert_eq!(
            report["workloads"][name]["delta"]["p50_percent"],
            json!(9900.0)
        );
        assert_eq!(report["workloads"][name]["delta"]["p95_ns"], json!(297));
    }
    Ok(())
}

#[test]
fn unsorted_p95_uses_nearest_rank_not_maximum_or_interpolation() -> TestResult {
    let baseline: Vec<u64> = (1..=20).rev().collect();
    let candidate: Vec<u64> = (1..=20).rev().map(|value| value * 2).collect();
    let fixture = Fixture::new(
        &measurement(&[baseline], 'a'),
        &measurement(&[candidate], 'a'),
    )?;
    let report = fixture.run_success()?;
    let workload = &report["workloads"]["hover_warm"];
    assert_eq!(workload["baseline"]["p50_ns"], json!(10));
    assert_eq!(workload["baseline"]["p95_ns"], json!(19));
    assert_eq!(workload["candidate"]["p95_ns"], json!(38));
    Ok(())
}

fn remove_field(value: &mut Value, key: &str) {
    if let Some(object) = value.as_object_mut() {
        object.remove(key);
    }
}

const INVALID_MUTATIONS: &[Mutation] = &[
    ("running collection", |data| {
        data["metadata"]["collection_status"] = json!("running");
    }),
    ("failed collection", |data| {
        data["metadata"]["collection_status"] = json!("failed");
    }),
    ("missing workload", |data| {
        remove_field(&mut data["workloads"], "hover_warm");
    }),
    ("extra workload", |data| {
        data["workloads"]["extra"] = json!({"rounds_ns": [[1, 2]]});
    }),
    ("digest mismatch", |data| {
        data["workload_digest"] = json!("e".repeat(64));
    }),
    ("malformed digest", |data| {
        data["workload_digest"] = json!("D".repeat(64));
    }),
    ("invalid binary hash", |data| {
        data["metadata"]["binary_sha256"] = json!("bad");
    }),
    ("wrong version", |data| data["format_version"] = json!(2)),
    ("boolean version", |data| {
        data["format_version"] = json!(true);
    }),
    ("missing metadata", |data| remove_field(data, "metadata")),
    ("nonobject metadata", |data| data["metadata"] = json!([])),
    ("nonobject workloads", |data| data["workloads"] = json!([])),
    ("missing samples", |data| {
        remove_field(&mut data["metadata"], "samples");
    }),
    ("zero rounds", |data| data["metadata"]["rounds"] = json!(0)),
    ("boolean samples", |data| {
        data["metadata"]["samples"] = json!(true);
    }),
    ("negative warmup", |data| {
        data["metadata"]["warmup"] = json!(-1);
    }),
    ("boolean warmup", |data| {
        data["metadata"]["warmup"] = json!(false);
    }),
    ("round count", |data| data["metadata"]["rounds"] = json!(2)),
    ("sample count", |data| {
        data["metadata"]["samples"] = json!(3);
    }),
    ("empty rounds", |data| {
        data["workloads"]["hover_warm"]["rounds_ns"] = json!([]);
    }),
    ("empty samples", |data| {
        data["workloads"]["hover_warm"]["rounds_ns"] = json!([[]]);
    }),
    ("missing measurements", |data| {
        data["workloads"]["hover_warm"] = json!({});
    }),
    ("nonobject workload", |data| {
        data["workloads"]["hover_warm"] = json!([]);
    }),
    ("boolean measurement", |data| {
        data["workloads"]["hover_warm"]["rounds_ns"] = json!([[true, 2]]);
    }),
    ("zero measurement", |data| {
        data["workloads"]["hover_warm"]["rounds_ns"] = json!([[0, 2]]);
    }),
    ("negative measurement", |data| {
        data["workloads"]["hover_warm"]["rounds_ns"] = json!([[-1, 2]]);
    }),
    ("float measurement", |data| {
        data["workloads"]["hover_warm"]["rounds_ns"] = json!([[1.5, 2]]);
    }),
    ("string measurement", |data| {
        data["workloads"]["hover_warm"]["rounds_ns"] = json!([["1", 2]]);
    }),
    ("platform mismatch", |data| {
        data["metadata"]["platform"] = json!("other");
    }),
    ("machine mismatch", |data| {
        data["metadata"]["machine"] = json!("other");
    }),
    ("harness mismatch", |data| {
        data["metadata"]["harness"] = json!("other");
    }),
    ("warmup mismatch", |data| {
        data["metadata"]["warmup"] = json!(1);
    }),
    ("missing identity", |data| {
        remove_field(&mut data["metadata"], "binary_sha256");
    }),
    ("missing digest", |data| {
        remove_field(data, "workload_digest");
    }),
    ("missing workloads", |data| remove_field(data, "workloads")),
    ("nonobject root", |data| *data = json!([])),
    ("zero samples", |data| {
        data["metadata"]["samples"] = json!(0);
    }),
    ("boolean rounds", |data| {
        data["metadata"]["rounds"] = json!(true);
    }),
    ("float rounds", |data| {
        data["metadata"]["rounds"] = json!(1.0);
    }),
    ("nested nonarray round", |data| {
        data["workloads"]["hover_warm"]["rounds_ns"] = json!([{}]);
    }),
];

#[test]
fn invalid_or_incomparable_measurements_fail_without_reports() -> TestResult {
    for &(case, mutate) in INVALID_MUTATIONS {
        for baseline_side in [true, false] {
            let mut baseline = measurement(&[vec![1, 2]], 'a');
            let mut candidate = measurement(&[vec![1, 2]], 'a');
            mutate(if baseline_side {
                &mut baseline
            } else {
                &mut candidate
            });
            let fixture = Fixture::new(&baseline, &candidate)?;
            fixture.assert_failure(&fixture.command().output()?, case);
        }
    }
    Ok(())
}

#[test]
fn environment_and_counts_are_required_and_strictly_typed() -> TestResult {
    for field in [
        "platform", "machine", "harness", "rounds", "samples", "warmup",
    ] {
        for invalid in [
            Value::Null,
            json!(""),
            json!(" \t\n"),
            json!([]),
            json!({}),
            json!(1.5),
        ] {
            for baseline_side in [true, false] {
                let mut baseline = measurement(&[vec![1, 2]], 'a');
                let mut candidate = measurement(&[vec![1, 2]], 'a');
                let data = if baseline_side {
                    &mut baseline
                } else {
                    &mut candidate
                };
                data["metadata"][field] = invalid.clone();
                let fixture = Fixture::new(&baseline, &candidate)?;
                fixture.assert_failure(&fixture.command().output()?, field);
            }
        }
        for baseline_side in [true, false] {
            let mut baseline = measurement(&[vec![1, 2]], 'a');
            let mut candidate = measurement(&[vec![1, 2]], 'a');
            let data = if baseline_side {
                &mut baseline
            } else {
                &mut candidate
            };
            remove_field(&mut data["metadata"], field);
            let fixture = Fixture::new(&baseline, &candidate)?;
            fixture.assert_failure(&fixture.command().output()?, field);
        }
    }
    Ok(())
}

#[test]
fn malformed_json_and_missing_files_fail() -> TestResult {
    for baseline_side in [true, false] {
        for missing in [true, false] {
            let fixture =
                Fixture::new(&measurement(&[vec![1]], 'a'), &measurement(&[vec![2]], 'a'))?;
            let path = if baseline_side {
                &fixture.baseline
            } else {
                &fixture.candidate
            };
            if missing {
                fs::remove_file(path)?;
            } else {
                fs::write(path, "{not json")?;
            }
            fixture.assert_failure(&fixture.command().output()?, "invalid input file");
        }
    }
    Ok(())
}

#[test]
fn paired_round_deltas_retain_alternating_collection_order() -> TestResult {
    let mut baseline = measurement(&[vec![100], vec![10], vec![40]], 'a');
    let mut candidate = measurement(&[vec![50], vec![30], vec![100]], 'b');
    let order = json!([
        ["baseline", "candidate"],
        ["candidate", "baseline"],
        ["baseline", "candidate"]
    ]);
    baseline["metadata"]["round_order"] = order.clone();
    candidate["metadata"]["round_order"] = order.clone();
    let fixture = Fixture::new(&baseline, &candidate)?;
    let report = fixture.run_success()?;
    let delta = &report["workloads"]["hover_warm"]["delta"];
    assert_eq!(
        delta["rounds"],
        json!([
            {"p50_ns": -50, "p95_ns": -50, "p50_percent": -50.0, "p95_percent": -50.0},
            {"p50_ns": 20, "p95_ns": 20, "p50_percent": 200.0, "p95_percent": 200.0},
            {"p50_ns": 60, "p95_ns": 60, "p50_percent": 150.0, "p95_percent": 150.0},
        ])
    );
    // Difference of aggregate medians is 10, not the median paired delta (20).
    assert_eq!(delta["p50_ns"], json!(10));
    assert_eq!(report["baseline_metadata"]["round_order"], order);
    assert_eq!(report["candidate_metadata"]["round_order"], order);
    Ok(())
}

#[test]
fn large_integer_and_half_nanosecond_summaries_are_exact() -> TestResult {
    let fixture = Fixture::new(
        &measurement(&[vec![u64::MAX], vec![u64::MAX - 1]], 'a'),
        &measurement(&[vec![1], vec![2]], 'b'),
    )?;
    let report = fixture.run_success()?;
    let workload = &report["workloads"]["hover_warm"];
    assert_eq!(
        workload["baseline"]["p50_ns"].to_string(),
        "18446744073709551614.5"
    );
    assert_eq!(workload["candidate"]["p95_ns"].to_string(), "1.5");
    assert_eq!(
        workload["delta"]["p50_ns"].to_string(),
        "-18446744073709551613"
    );
    assert_eq!(
        workload["delta"]["rounds"][0]["p95_ns"].to_string(),
        "-18446744073709551614"
    );
    let half_delta = Fixture::new(
        &measurement(&[vec![1], vec![2]], 'a'),
        &measurement(&[vec![1], vec![1]], 'b'),
    )?;
    let report = half_delta.run_success()?;
    assert_eq!(
        report["workloads"]["hover_warm"]["delta"]["p50_ns"].to_string(),
        "-0.5"
    );
    Ok(())
}

#[test]
fn latency_cli_requires_outputs_and_rejects_unknown_flags() -> TestResult {
    let fixture = Fixture::new(&measurement(&[vec![1]], 'a'), &measurement(&[vec![2]], 'a'))?;
    for output_flag in ["--json-output", "--markdown-output"] {
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args(["reports", "latency"])
            .arg(&fixture.baseline)
            .arg(&fixture.candidate)
            .arg(output_flag)
            .arg(fixture.directory.path().join("unused"))
            .output()?;
        fixture.assert_failure(&output, "missing required output");
    }
    fixture.assert_failure(
        &fixture
            .command()
            .arg("--latency-threshold")
            .arg("10")
            .output()?,
        "unknown threshold flag",
    );
    Ok(())
}

#[test]
fn reports_reject_nonfinite_json_on_either_side() -> TestResult {
    for side in ["baseline.json", "candidate.json"] {
        for token in ["NaN", "Infinity", "-Infinity", "1e999"] {
            let fixture =
                Fixture::new(&measurement(&[vec![1]], 'a'), &measurement(&[vec![2]], 'a'))?;
            let path: &Path = fixture.directory.path();
            fs::write(path.join(side), format!("{{\"format_version\": {token}}}"))?;
            fixture.assert_failure(&fixture.command().output()?, token);
        }
    }
    Ok(())
}

#[test]
fn sample_integers_outside_u64_are_rejected_on_either_side() -> TestResult {
    let overflowing: Value = serde_json::from_str("[[18446744073709551616,2]]")?;
    for baseline_side in [true, false] {
        let mut baseline = measurement(&[vec![1, 2]], 'a');
        let mut candidate = measurement(&[vec![1, 2]], 'a');
        let data = if baseline_side {
            &mut baseline
        } else {
            &mut candidate
        };
        data["workloads"]["hover_warm"]["rounds_ns"] = overflowing.clone();
        let fixture = Fixture::new(&baseline, &candidate)?;
        fixture.assert_failure(&fixture.command().output()?, "sample outside u64");
    }
    Ok(())
}

#[test]
fn latency_help_succeeds_without_collecting_measurements() -> TestResult {
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["reports", "latency", "--help"])
        .env_remove("GITHUB_ACTIONS")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let help = String::from_utf8(output.stdout)?;
    assert!(help.contains("--json-output"));
    assert!(help.contains("--markdown-output"));
    Ok(())
}
