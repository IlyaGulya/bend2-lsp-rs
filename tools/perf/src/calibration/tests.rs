mod reports;

use super::{
    Arguments, Identity, METRICS, PROBE, VARIANTS,
    collector::{balanced_order, variant_harness},
    integrity::*,
};
use crate::ToolResult;
use clap::Parser as _;
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, fs, path::Path};

const EXECUTABLE: &str = "/calibration/a/target/release/deps/analysis-actual";

fn fresh_summary(counts: [Value; 3]) -> Value {
    let metrics: Map<_, _> = METRICS
        .into_iter()
        .zip(counts)
        .map(|(metric, value)| {
            (
                metric.to_owned(),
                json!({"metrics": {"Left": {"Int": value}}}),
            )
        })
        .collect();
    json!({"function_name": "inlay_hints_warm", "id": "small", "baselines": ["fresh123", "fresh123"], "kind": "LibraryBenchmark", "benchmark_exe": EXECUTABLE, "profiles": [{"tool": "Callgrind", "summaries": {"parts": [{"metrics_summary": {"Callgrind": metrics}}]}}]})
}

fn expected() -> BTreeSet<Identity> {
    BTreeSet::from([("inlay_hints_warm".to_owned(), Some("small".to_owned()))])
}

fn measured(summaries: &[Value], expected: &BTreeSet<Identity>) -> ToolResult<Vec<Value>> {
    let text = summaries
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    Ok(parse_measurement(&text, "fresh123", Path::new(EXECUTABLE), expected)?.0)
}

fn assert_error<T>(result: ToolResult<T>, message: &str) -> ToolResult<()> {
    let error = result.err().ok_or("expected error")?;
    assert!(error.to_string().contains(message), "{error}");
    Ok(())
}

#[test]
fn zero_is_a_real_count_not_a_missing_measurement() -> ToolResult<()> {
    assert_eq!(
        measured(
            &[fresh_summary([json!(0), json!(0), json!(0)])],
            &expected()
        )?,
        vec![
            json!({"function_name": "inlay_hints_warm", "id": "small", "counts": {"Ir": 0, "I1mr": 0, "ILmr": 0}})
        ]
    );
    Ok(())
}

#[test]
fn invalid_count_types_and_signs_are_rejected() -> ToolResult<()> {
    for index in 0..3 {
        for invalid in [
            json!(-1),
            json!(true),
            json!(false),
            json!(1.0),
            json!("1"),
            Value::Null,
        ] {
            let mut counts = [json!(100), json!(2), json!(3)];
            counts[index] = invalid;
            assert_error(
                measured(&[fresh_summary(counts)], &expected()),
                "nonnegative integer",
            )?;
        }
    }
    Ok(())
}

#[test]
fn missing_duplicate_and_extra_workloads_are_rejected() -> ToolResult<()> {
    assert_error(measured(&[], &expected()), "missing workloads")?;
    let summary = fresh_summary([json!(100), json!(2), json!(3)]);
    assert_error(
        measured(&[summary.clone(), summary.clone()], &expected()),
        "duplicate or unexpected",
    )?;
    let mut extra = summary.clone();
    extra["id"] = json!("unexpected");
    assert_error(measured(&[extra], &expected()), "duplicate or unexpected")?;
    let mut ids = expected();
    ids.insert(("inlay_hints_warm".to_owned(), Some("medium".to_owned())));
    assert_error(measured(&[summary], &ids), "missing workloads")
}

#[test]
fn null_workload_id_is_distinct_from_named_id() -> ToolResult<()> {
    let mut summary = fresh_summary([json!(100), json!(2), json!(3)]);
    summary["id"] = Value::Null;
    assert_eq!(
        measured(
            &[summary.clone()],
            &BTreeSet::from([("inlay_hints_warm".to_owned(), None)])
        )?[0]["id"],
        Value::Null
    );
    assert_error(measured(&[summary], &expected()), "duplicate or unexpected")
}

#[test]
fn stale_baseline_or_other_executable_cannot_supply_metrics() {
    for (field, value) in [
        ("baselines", json!([null, "fresh123"])),
        ("baselines", json!(["stale", "stale"])),
        ("benchmark_exe", json!("/old/analysis")),
        ("kind", json!("BinaryBenchmark")),
    ] {
        let mut summary = fresh_summary([json!(100), json!(2), json!(3)]);
        summary[field] = value;
        assert!(measured(&[summary], &expected()).is_err());
    }
}

#[test]
fn comparison_metrics_cannot_be_mistaken_for_fresh_counts() -> ToolResult<()> {
    for value in [
        json!({"Both": [{"Int": 100}, {"Int": 100}]}),
        json!({"Right": {"Int": 100}}),
        json!({"Left": {"Int": 100}, "Both": [{"Int": 100}, {"Int": 100}]}),
    ] {
        let mut summary = fresh_summary([json!(100), json!(2), json!(3)]);
        summary["profiles"][0]["summaries"]["parts"][0]["metrics_summary"]["Callgrind"]["Ir"]["metrics"] =
            value;
        assert_error(measured(&[summary], &expected()), "Left-only")?;
    }
    Ok(())
}

#[test]
fn missing_metrics_and_unexpected_profile_parts_fail_closed() -> ToolResult<()> {
    for mutation in ["metric", "part", "tool", "profile", "malformed"] {
        let mut summary = fresh_summary([json!(100), json!(2), json!(3)]);
        match mutation {
            "metric" => {
                summary["profiles"][0]["summaries"]["parts"][0]["metrics_summary"]["Callgrind"]
                    .as_object_mut()
                    .ok_or("metrics missing")?
                    .remove("ILmr");
            }
            "part" => {
                let part = summary["profiles"][0]["summaries"]["parts"][0].clone();
                summary["profiles"][0]["summaries"]["parts"]
                    .as_array_mut()
                    .ok_or("parts missing")?
                    .push(part);
            }
            "tool" => summary["profiles"][0]["tool"] = json!("Cachegrind"),
            "malformed" => summary["profiles"] = json!([true]),
            _ => {
                let profile = summary["profiles"][0].clone();
                summary["profiles"]
                    .as_array_mut()
                    .ok_or("profiles missing")?
                    .push(profile);
            }
        }
        assert!(measured(&[summary], &expected()).is_err());
    }
    Ok(())
}

#[test]
fn non_json_transport_duplicate_keys_and_nonfinite_numbers_fail_closed() {
    assert!(
        parse_measurement(
            "profiler failed\n",
            "fresh123",
            Path::new(EXECUTABLE),
            &expected()
        )
        .is_err()
    );
    for text in [
        "{\"Ir\":1,\"Ir\":2}",
        "{\"count\":NaN}",
        "{\"count\":Infinity}",
        "{\"count\":1e999}",
        "{\"count\": {\"Ir\":1,\"\\u0049r\":2}}",
        "[1,]",
        "{\"x\":1,}",
        "\u{000b}1",
    ] {
        assert!(strict_json(text).is_err(), "{text}");
    }
}

#[test]
fn arbitrary_precision_and_private_number_key_preserve_real_json_types() -> ToolResult<()> {
    assert_eq!(
        strict_json("-18446744073709551615")?.to_string(),
        "-18446744073709551615"
    );
    let literal = strict_json("{\"$serde_json::private::Number\":\"1\"}")?;
    assert!(literal.is_object());
    assert!(strict_json("1.0")?.as_u64().is_none());
    Ok(())
}

#[test]
fn real_nm_format_reports_address_size_and_demangled_name() -> ToolResult<()> {
    assert_eq!(
        parse_probe_symbols(
            &format!("0000000000012340 0000000000000400 t analysis::{PROBE}\n"),
            true
        )?,
        json!({"symbol": format!("analysis::{PROBE}"), "address": "0000000000012340", "size": 1024})
    );
    Ok(())
}

#[test]
fn unretained_ambiguous_small_or_unsupported_symbols_fail() {
    let record = format!("00012340 00000400 t analysis::{PROBE}\n");
    for text in [
        String::new(),
        format!("{record}{record}"),
        record.replace("00000400", "000003ff"),
        record.replace("00012340", "00000000"),
        record.replace(" t ", " U "),
        record.replace(PROBE, &format!("{PROBE}_other")),
        format!("00012340 t analysis::{PROBE}\n"),
    ] {
        assert!(parse_probe_symbols(&text, true).is_err(), "{text}");
    }
    assert!(parse_probe_symbols(&record.replace("00000400", "00000000"), false).is_err());
}

#[test]
fn collected_probe_and_malformed_profiles_fail() -> ToolResult<()> {
    let header = "# callgrind format\nversion: 1\nevents: Ir I1mr ILmr\n";
    check_probe_absent(&format!(
        "{header}fn=(1) analysis::inlay_hints_warm\n1 100 2 3\n"
    ))?;
    for text in [
        format!("{header}fn=(2) analysis::{PROBE}\n1 1 0 0\n"),
        format!("{header}cfn=(2) analysis::{PROBE}\ncalls=1 1\n1 1 0 0\n"),
        String::new(),
        "events: Ir\n".to_owned(),
        header.replace("events: Ir", "events: Dr"),
    ] {
        assert!(check_probe_absent(&text).is_err());
    }
    Ok(())
}

#[test]
fn exact_known_anchors_reject_harness_drift_and_ambiguity() -> ToolResult<()> {
    let harness = include_str!("../../../../benches/analysis.rs");
    let alterations = [
        harness.replace("fn inlay_hints_warm(", "fn renamed_inlay_hints_warm("),
        harness.replace(
            "identifier_name: \"transform_31\"",
            "identifier_name: \"different\"",
        ),
        harness.replace("fn medium() -> Self", "fn medium_changed() -> Self"),
        harness.replace("fn large() -> Self", "fn large_changed() -> Self"),
        format!("{harness}{harness}"),
        format!("{harness}\nfn {PROBE}(state: u64) -> u64 {{ state }}\n"),
    ];
    for variant in VARIANTS {
        variant_harness(harness, variant)?;
        for altered in &alterations {
            assert!(variant_harness(altered, variant).is_err(), "{variant}");
        }
    }
    assert_eq!(
        variant_harness(harness, "a")?,
        variant_harness(harness, "b")?
    );
    assert!(variant_harness(harness, "unknown").is_err());
    Ok(())
}

#[test]
fn cargo_artifact_is_required_and_cannot_escape_independent_target() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let target = root.join("target");
    fs::create_dir(&target)?;
    let executable = target.join("compiler-chosen-name");
    fs::write(&executable, b"executable artifact")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    }
    let artifact = json!({"reason": "compiler-artifact", "target": {"name": "analysis", "kind": ["bench"]}, "executable": executable});
    let finished = json!({"reason": "build-finished", "success": true});
    let messages = |rows: &[Value]| {
        rows.iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map(|rows| rows.join("\n"))
    };
    assert_eq!(
        parse_executable(
            &messages(&[artifact.clone(), finished.clone()])?,
            &root,
            &target
        )?,
        executable.canonicalize()?
    );
    let mut outside = artifact.clone();
    outside["executable"] = json!(root.join("old-executable"));
    for rows in [
        vec![finished.clone()],
        vec![artifact.clone()],
        vec![artifact.clone(), artifact.clone(), finished.clone()],
        vec![outside, finished.clone()],
        vec![
            artifact.clone(),
            json!({"reason": "build-finished", "success": false}),
        ],
        vec![artifact.clone(), finished.clone(), finished],
    ] {
        assert!(parse_executable(&messages(&rows)?, &root, &target).is_err());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(executable, fs::Permissions::from_mode(0o644))?;
        assert!(
            parse_executable(
                &messages(&[
                    artifact,
                    json!({"reason": "build-finished", "success": true})
                ])?,
                &root,
                &target
            )
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn each_five_pair_block_balances_all_execution_positions() -> ToolResult<()> {
    for job in ["discovery1", "validation3", "local"] {
        for first in [1, 6] {
            let orders = (first..first + 5)
                .map(|pair| balanced_order(job, pair))
                .collect::<ToolResult<Vec<_>>>()?;
            for variant in VARIANTS {
                let mut positions = orders
                    .iter()
                    .map(|order| {
                        order
                            .iter()
                            .position(|item| *item == variant)
                            .ok_or("variant missing")
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                positions.sort_unstable();
                assert_eq!(positions, vec![0, 1, 2, 3, 4]);
            }
        }
    }
    Ok(())
}

#[test]
fn nested_or_stale_destinations_cannot_contaminate_source_or_output() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let source = root.join("source");
    fs::create_dir(&source)?;
    let work = root.join("work");
    let output = root.join("output");
    validate_directories(&source, &work, &output)?;
    validate_directories(&source, &source.join("work"), &source.join("output"))?;
    for (work, output) in [
        (root.to_owned(), output.clone()),
        (work.clone(), root.to_owned()),
        (work.clone(), work.join("raw")),
        (output.join("build"), output.clone()),
        (source.clone(), output.clone()),
    ] {
        assert!(validate_directories(&source, &work, &output).is_err());
    }
    fs::create_dir(&work)?;
    fs::write(work.join("old-binary"), b"stale")?;
    assert_error(validate_directories(&source, &work, &output), "stale")
}

#[cfg(unix)]
#[test]
fn snapshot_rejects_symlinks_to_mutable_external_inputs() -> ToolResult<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source");
    fs::create_dir(&source)?;
    let external = directory.path().join("changing.rs");
    fs::write(&external, "external mutable source")?;
    std::os::unix::fs::symlink(external, source.join("library.rs"))?;
    assert_error(source_manifest(&source, &[]), "symlinks")
}

#[test]
fn executed_workload_manifest_rejects_incomplete_or_duplicate_list() -> ToolResult<()> {
    let lines: Vec<_> = ["small", "medium", "large"]
        .map(|size| format!("analysis::analysis_hot_paths::inlay_hints_warm::{size}: benchmark"))
        .into_iter()
        .collect();
    let valid = format!("{}\n\n0 tests, 3 benchmarks", lines.join("\n"));
    assert_eq!(parse_workload_list(&valid)?, signals());
    for text in [
        valid.replace("3 benchmarks", "4 benchmarks"),
        format!("{valid}\n{}", lines[0]),
        format!("{}\n0 tests, 2 benchmarks", lines[..2].join("\n")),
        format!("{valid}\nprofiler warning"),
    ] {
        assert!(parse_workload_list(&text).is_err());
    }
    Ok(())
}

#[test]
fn collector_cli_preserves_five_pair_default_and_strict_options() -> ToolResult<()> {
    let args = [
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
    let super::Command::Collect(parsed) = Arguments::try_parse_from(args)?.command else {
        return Err("wrong command".into());
    };
    assert_eq!(parsed.pairs, 5);
    for extra in [
        ["--pairs", "0"],
        ["--pairs", "-1"],
        ["--role", "other"],
        ["--job-id", " "],
    ] {
        assert!(Arguments::try_parse_from(args.into_iter().chain(extra)).is_err());
    }
    Ok(())
}
