use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};
use std::{fmt::Write as _, fs, path::Path};

pub type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write(root: &Path, relative: &str, bytes: &[u8]) -> TestResult {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().ok_or("missing fixture parent")?)?;
    fs::write(path, bytes)?;
    Ok(())
}

fn workloads(benchmark: &str) -> Vec<Value> {
    let function = if benchmark == "analysis" {
        "inlay_hints_warm"
    } else {
        "reference_location_order_warm"
    };
    ["small", "medium", "large"]
        .into_iter()
        .map(|id| json!({"function_name": function, "id": id}))
        .collect()
}

fn inventory(root: &Path, directory: &Path, files: &mut Vec<Value>) -> TestResult {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            inventory(root, &path, files)?;
        } else if path != root.join("artifact-manifest.json") {
            let bytes = fs::read(&path)?;
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .ok_or("non-UTF8 fixture path")?
                .replace('\\', "/");
            files.push(json!({"path": relative, "sha256": digest(&bytes), "size": bytes.len()}));
        }
    }
    Ok(())
}

pub fn refresh_inventory(root: &Path, manifest: &mut Value) -> TestResult {
    let mut files = Vec::new();
    inventory(root, root, &mut files)?;
    files.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    manifest["files"] = json!(files);
    let mut expected = vec![
        json!({"path": "callgrind-policy.json", "kind": "callgrind", "required": true,
        "present": root.join("callgrind-policy.json").is_file()}),
    ];
    expected.extend(files.iter().filter(|file| file["path"] != "callgrind-policy.json" && file["path"] != "callgrind-policy.md")
        .map(|file| json!({"path": file["path"], "kind": "callgrind-input", "required": true, "present": true})));
    manifest["expected"] = json!(expected);
    manifest["expected_targets"] = json!(["x86_64-unknown-linux-gnu"]);
    write(
        root,
        "artifact-manifest.json",
        &serde_json::to_vec(manifest)?,
    )
}

fn build_variants(root: &Path, provenance: &Value) -> TestResult<Map<String, Value>> {
    let mut variants = Map::new();
    for variant in ["a", "b"] {
        let source_files = json!([
            {"path": "benches/analysis.rs", "sha256": digest(b"original analysis fixture source"), "executable": false},
            {"path": "benches/reference_locations.rs", "sha256": digest(b"original references fixture source"), "executable": false}
        ]);
        let source_hash = digest(&serde_json::to_vec(&source_files)?);
        let source_path = format!("manifests/{variant}.json");
        write(
            root,
            &source_path,
            &serde_json::to_vec(&json!({"sha256": source_hash, "files": source_files}))?,
        )?;
        let mut benchmarks = Map::new();
        for benchmark in ["analysis", "reference_locations"] {
            let ids = workloads(benchmark);
            let group = if benchmark == "analysis" {
                "analysis_hot_paths"
            } else {
                "reference_order_hot_paths"
            };
            let mut list = String::new();
            for id in &ids {
                let function = id["function_name"]
                    .as_str()
                    .ok_or("fixture function missing")?;
                let suffix = id["id"]
                    .as_str()
                    .map_or_else(String::new, |id| format!("::{id}"));
                writeln!(list, "{benchmark}::{group}::{function}{suffix}: benchmark")?;
            }
            writeln!(list, "\n0 tests, {} benchmarks", ids.len())?;
            let list_path = format!("builds/{variant}/{benchmark}.list.stdout");
            write(root, &list_path, list.as_bytes())?;
            // A backslash is a literal Linux filename character, not a Windows separator.
            benchmarks.insert(
                benchmark.to_owned(),
                json!({
                    "executable": format!("/isolated\\build/{variant}/{benchmark}"),
                    "binary_sha256": digest(format!("{variant}:{benchmark}:binary").as_bytes()),
                    "workloads": ids, "workload_list": list_path
                }),
            );
        }
        variants.insert(variant.to_owned(), json!({
            "source_revision": provenance[if variant == "a" { "base_sha" } else { "candidate_sha" }],
            "source_sha256": source_hash, "source_manifest": source_path, "benchmarks": benchmarks
        }));
    }
    Ok(variants)
}

pub fn write_fixture(
    root: &Path,
    baseline_ir: u64,
    candidate_ir: impl Fn(u64) -> u64,
) -> TestResult<Value> {
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let provenance = json!({
        "target": "x86_64-unknown-linux-gnu", "base_sha": "a".repeat(40),
        "candidate_sha": "b".repeat(40), "workflow_sha": "c".repeat(40),
        "repository": "owner/repository", "request_id": "fixture-run-0001",
        "run_id": "12345", "run_attempt": "1"
    });
    let gate = json!({"workflow": "performance", "job": "compare",
        "step": "Compare pull request with main", "head_sha": "b".repeat(40)});
    let variants = build_variants(&root, &provenance)?;
    let mut samples = Vec::new();
    for pair in 1..=105 {
        for variant in ["a", "b"] {
            let ir = if variant == "a" {
                baseline_ir
            } else {
                candidate_ir(pair)
            };
            let baseline = digest(format!("fixture-job:{pair}:{variant}").as_bytes());
            let mut measured = Vec::new();
            let mut benchmarks = Map::new();
            for benchmark in ["analysis", "reference_locations"] {
                let directory = format!("raw/pair{pair:03}{variant}/{benchmark}");
                let stdout = format!("{directory}/stdout.jsonl");
                let stderr = format!("{directory}/stderr.log");
                let ids = workloads(benchmark);
                let mut text = String::new();
                let (ir, cache) = if benchmark == "analysis" {
                    (ir, 10)
                } else {
                    (2000, 20)
                };
                for (index, id) in ids.iter().enumerate() {
                    let profile = format!("{directory}/iai/callgrind.{index}.out");
                    write(&root, &profile, format!("# callgrind format\nversion: 1\nevents: Ir I1mr ILmr\nsummary: {ir} {cache} {cache}\nfn={}\n1 {ir} {cache} {cache}\ntotals: {ir} {cache} {cache}\n", id["function_name"].as_str().ok_or("fixture function missing")?).as_bytes())?;
                    let counts = json!({"Ir": ir, "I1mr": cache, "ILmr": cache});
                    let metrics: Map<_, _> = counts
                        .as_object()
                        .ok_or("fixture counts missing")?
                        .iter()
                        .map(|(metric, count)| {
                            (metric.clone(), json!({"metrics": {"Left": {"Int": count}}}))
                        })
                        .collect();
                    let summary = json!({
                        "function_name": id["function_name"], "id": id["id"], "kind": "LibraryBenchmark",
                        "benchmark_exe": variants[variant]["benchmarks"][benchmark]["executable"],
                        "baselines": [baseline, baseline],
                        "profiles": [{"tool": "Callgrind", "out_paths": [format!("/home/runner\\work/paired-evidence/{profile}")],
                            "summaries": {"parts": [{"metrics_summary": {"Callgrind": metrics}}]}}]
                    });
                    text.push_str(&serde_json::to_string(&summary)?);
                    text.push('\n');
                    measured.push(json!({"function_name": id["function_name"], "id": id["id"], "counts": counts}));
                }
                write(&root, &stdout, text.as_bytes())?;
                write(&root, &stderr, b"")?;
                benchmarks.insert(
                    benchmark.to_owned(),
                    json!({"raw_stdout": stdout, "raw_stderr": stderr,
                    "verified_callgrind_profiles": ids.len()}),
                );
            }
            samples.push(json!({"pair": pair, "variant": variant,
                "order": usize::from((pair % 2 == 1) != (variant == "a")),
                "workloads": measured, "benchmarks": benchmarks}));
        }
    }
    let data = json!({"format_version": 1, "kind": "paired-callgrind", "method": "paired-median-105-v1",
        "pairs": 105, "job_id": "fixture-job", "provenance": provenance, "gate_identity": gate,
        "environment": {"rustc": "rustc 1.98.1", "cargo": "cargo 1.98.1", "iai_runner": "iai-callgrind-runner 0.16.1",
            "valgrind": "valgrind-3.27.1", "os": "Linux", "arch": "x86_64", "cache_args": ["--cache-sim=yes"]},
        "host": {"runner_identity": "fixture-runner", "host_cpu": "fixture-cpu"},
        "variants": variants, "samples": samples});
    write(&root, "paired-data.json", &serde_json::to_vec(&data)?)?;
    let mut manifest = provenance;
    manifest["schema_version"] = json!(1);
    manifest["mode"] = json!("callgrind");
    manifest["measurement_method"] = json!("paired-median-105-v1");
    manifest["gate_identity"] = gate;
    refresh_inventory(&root, &mut manifest)?;
    Ok(manifest)
}
