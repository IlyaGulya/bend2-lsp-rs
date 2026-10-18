#[path = "support/paired.rs"]
mod paired;

use paired::{TestResult, write_fixture};
use serde_json::{Value, json};
use std::{fs, process::Command};

#[test]
fn paired_report_accepts_an_isolated_outlier_with_complete_105_sample_bounds() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    write_fixture(root, 1000, |pair| if pair == 1 { 10_000 } else { 1000 })?;
    let verdict_path = root.join("callgrind-policy.json");
    let markdown_path = root.join("callgrind-policy.md");
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["paired", "report"])
        .arg(root)
        .arg("--json-output")
        .arg(&verdict_path)
        .arg("--markdown-output")
        .arg(&markdown_path)
        .output()?;
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let verdict: Value = serde_json::from_slice(&fs::read(verdict_path)?)?;
    assert_eq!(verdict["status"], "complete");
    assert_eq!(verdict["method"], "paired-median-105-v1");
    assert_eq!(verdict["workload_count"], 6);
    assert_eq!(verdict["passed_count"], 6);
    assert_eq!(verdict["failures"], json!([]));
    assert_eq!(verdict["new_workloads"], json!([]));
    let workloads = verdict["workloads"]
        .as_array()
        .ok_or("missing verdict workloads")?;
    assert_eq!(workloads.len(), 6);
    for (function, denominator, exceedances) in [
        ("inlay_hints_warm", 1000, 1),
        ("reference_location_order_warm", 2000, 0),
    ] {
        let workload = workloads
            .iter()
            .find(|row| row["function_name"] == function && row["id"] == "small")
            .ok_or("missing benchmark's small workload")?;
        let comparison = &workload["metrics"]["Ir"];
        assert_eq!(comparison["classification"], "passed");
        assert_eq!(comparison["samples"], 105);
        assert!(
            comparison["lower"]["numerator"]
                .as_i64()
                .is_some_and(|value| value < 0)
        );
        assert_eq!(comparison["lower"]["denominator"], denominator);
        assert!(
            comparison["upper"]["numerator"]
                .as_i64()
                .is_some_and(|value| value < 0)
        );
        assert_eq!(comparison["raw_exceedances"], exceedances);
    }
    Ok(())
}

#[test]
fn paired_report_preserves_zero_baseline_counts_without_accepting_missing_instructions()
-> TestResult {
    for (baseline, expected_exit) in [(0, 0), (1000, 2)] {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        write_fixture(root, baseline, |_| 0)?;
        let verdict_path = root.join("callgrind-policy.json");
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args(["paired", "report"])
            .arg(root)
            .arg("--json-output")
            .arg(&verdict_path)
            .arg("--markdown-output")
            .arg(root.join("callgrind-policy.md"))
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "baseline={baseline}, stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if baseline == 0 {
            let verdict: Value = serde_json::from_slice(&fs::read(verdict_path)?)?;
            assert_eq!(verdict["status"], "complete");
            let workload = verdict["workloads"]
                .as_array()
                .ok_or("missing workloads")?
                .iter()
                .find(|row| row["function_name"] == "inlay_hints_warm" && row["id"] == "small")
                .ok_or("missing zero baseline workload")?;
            assert_eq!(
                workload["metrics"]["Ir"]["upper"],
                json!({"numerator": 0, "denominator": 1})
            );
            assert_eq!(workload["metrics"]["Ir"]["samples"], 105);
        }
    }
    Ok(())
}

#[test]
fn paired_report_requires_profile_counts_to_match_retained_stdout() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    write_fixture(root, 1000, |_| 1000)?;
    let profile = root.join("raw/pair001b/analysis/iai/callgrind.0.out");
    for (events, counts, expected_exit, expected_status) in [
        (
            "Ir I1mr ILmr",
            "summary: 0 0 0\ntotals: 1000 10 10\n",
            0,
            "complete",
        ),
        (
            "Ir I1mr ILmr",
            "summary: 1000 10 10\ntotals: 2000 10 10\n",
            2,
            "error",
        ),
        (
            "Ir I1mr ILmr",
            "summary: 1000 10 10\ntotals: 1000 11 10\n",
            2,
            "error",
        ),
        (
            "Ir I1mr ILmr",
            "summary: 1000 10 10\ntotals: 1000 10 11\n",
            2,
            "error",
        ),
        ("Ir I1mr ILmr", "", 2, "error"),
        ("Ir I1mr ILmr", "summary: 1000 10 10\n", 0, "complete"),
        ("ILmr Dr Ir I1mr", "totals: 10 99 1000 10\n", 0, "complete"),
        ("Ir I1mr", "totals: 1000 10\n", 2, "error"),
        ("Ir I1mr ILmr Ir", "totals: 1000 10 10 1000\n", 2, "error"),
        ("Ir I1mr ILmr", "totals: 1000 10\n", 2, "error"),
        ("Ir I1mr ILmr", "totals: 1000 10 10 1\n", 2, "error"),
        ("Ir I1mr ILmr", "totals: -1000 10 10\n", 2, "error"),
        ("Ir I1mr ILmr", "totals: 1000 invalid 10\n", 2, "error"),
        (
            "Ir I1mr ILmr",
            "totals: 18446744073709551616 10 10\n",
            2,
            "error",
        ),
        (
            "Ir I1mr ILmr",
            "summary: 1000 10 10\nsummary: 2000 10 10\n",
            2,
            "error",
        ),
        (
            "Ir I1mr ILmr",
            "totals: 1000 10 10\ntotals: 2000 10 10\n",
            2,
            "error",
        ),
        (
            "Ir I1mr ILmr",
            "totals: 1000 10 10\nevents: Ir ILmr I1mr\n",
            2,
            "error",
        ),
        (
            "Ir I1mr ILmr",
            "totals: 1000 10 10\nfn=calibration_layout_probe\n",
            2,
            "error",
        ),
    ] {
        fs::write(
            &profile,
            format!("# callgrind format\nversion: 1\nevents: {events}\n{counts}"),
        )?;
        let verdict_path = root.join("callgrind-policy.json");
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args(["paired", "report"])
            .arg(root)
            .arg("--json-output")
            .arg(&verdict_path)
            .arg("--markdown-output")
            .arg(root.join("callgrind-policy.md"))
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(expected_exit),
            "{counts:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let verdict: Value = serde_json::from_slice(&fs::read(&verdict_path)?)?;
        assert_eq!(verdict["status"], expected_status);
    }
    Ok(())
}

#[test]
fn paired_report_accepts_native_event_columns_and_zero_counters() -> TestResult {
    for (ir, cache, counters) in [
        (34077, 106, "34077 11695 5361 106 2 53 106 0 53"),
        (61, 3, "61 18 11 3 0 0 3"),
        (0, 0, "0"),
        (0, 0, "0 0 0 0 0 0 0 0 0"),
    ] {
        let directory = tempfile::tempdir()?;
        let root = directory.path();
        write_fixture(root, ir, |_| ir)?;
        let stdout_path = root.join("raw/pair001b/analysis/stdout.jsonl");
        let mut summaries: Vec<Value> = fs::read_to_string(&stdout_path)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        for (metric, count) in [("Ir", ir), ("I1mr", cache), ("ILmr", cache)] {
            summaries[0]["profiles"][0]["summaries"]["parts"][0]["metrics_summary"]["Callgrind"]
                [metric]["metrics"]["Left"]["Int"] = json!(count);
        }
        fs::write(
            &stdout_path,
            summaries
                .iter()
                .map(serde_json::to_string)
                .collect::<Result<Vec<_>, _>>()?
                .join("\n"),
        )?;
        let dataset_path = root.join("paired-data.json");
        let mut dataset: Value = serde_json::from_slice(&fs::read(&dataset_path)?)?;
        let sample = dataset["samples"]
            .as_array_mut()
            .ok_or("missing samples")?
            .iter_mut()
            .find(|sample| sample["pair"] == 1 && sample["variant"] == "b")
            .ok_or("missing candidate sample")?;
        sample["workloads"][0]["counts"] = json!({"Ir": ir, "I1mr": cache, "ILmr": cache});
        fs::write(&dataset_path, serde_json::to_vec(&dataset)?)?;
        // Native Callgrind 3.27.1 column order and retained counters from run 38682489043.
        fs::write(
            root.join("raw/pair001b/analysis/iai/callgrind.0.out"),
            format!(
                "# callgrind format\nversion: 1\ncreator: callgrind-3.27.1\npid: 20157\npart: 1\nthread: 1\npositions: line\nevents: Ir Dr Dw I1mr D1mr D1mw ILmr DLmr DLmw\nsummary: {counters}\nfn=inlay_hints_warm\n1 {counters}\ntotals: {counters}\n"
            ),
        )?;
        let verdict_path = root.join("callgrind-policy.json");
        let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
            .args(["paired", "report"])
            .arg(root)
            .arg("--json-output")
            .arg(&verdict_path)
            .arg("--markdown-output")
            .arg(root.join("callgrind-policy.md"))
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let verdict: Value = serde_json::from_slice(&fs::read(&verdict_path)?)?;
        assert_eq!(verdict["status"], "complete");
    }
    Ok(())
}

#[test]
fn paired_report_rejects_multiple_retained_files_for_one_fresh_part() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    write_fixture(root, 1000, |_| 1000)?;
    let stdout_path = root.join("raw/pair001b/analysis/stdout.jsonl");
    let mut summaries: Vec<Value> = fs::read_to_string(&stdout_path)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let paths = summaries[0]["profiles"][0]["out_paths"]
        .as_array_mut()
        .ok_or("missing profile paths")?;
    let extra = paths[0]
        .as_str()
        .ok_or("missing profile path")?
        .replace("callgrind.0.out", "callgrind.extra.out");
    paths.push(json!(extra));
    fs::write(
        &stdout_path,
        summaries
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n"),
    )?;
    for name in ["callgrind.0.out", "callgrind.extra.out"] {
        fs::write(
            root.join("raw/pair001b/analysis/iai").join(name),
            "# callgrind format\nversion: 1\nevents: Ir I1mr ILmr\ntotals: 500 5 5\n",
        )?;
    }
    let dataset_path = root.join("paired-data.json");
    let mut dataset: Value = serde_json::from_slice(&fs::read(&dataset_path)?)?;
    let sample = dataset["samples"]
        .as_array_mut()
        .ok_or("missing samples")?
        .iter_mut()
        .find(|sample| sample["pair"] == 1 && sample["variant"] == "b")
        .ok_or("missing candidate sample")?;
    sample["benchmarks"]["analysis"]["verified_callgrind_profiles"] = json!(4);
    fs::write(&dataset_path, serde_json::to_vec(&dataset)?)?;
    let verdict_path = root.join("callgrind-policy.json");
    let output = Command::new(env!("CARGO_BIN_EXE_bend2-perf"))
        .args(["paired", "report"])
        .arg(root)
        .arg("--json-output")
        .arg(&verdict_path)
        .arg("--markdown-output")
        .arg(root.join("callgrind-policy.md"))
        .output()?;
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let verdict: Value = serde_json::from_slice(&fs::read(&verdict_path)?)?;
    assert_eq!(verdict["status"], "error");
    Ok(())
}
