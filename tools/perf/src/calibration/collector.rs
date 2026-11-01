use super::{BENCHMARKS, CollectArgs, PROBE, VARIANTS, require, shard};
use crate::{
    ToolResult,
    callgrind::{
        self,
        collect::{
            Build, Environment, clean_environment, command_strings, compile, copy_verified_source,
            ensure_immutable, environment_metadata, host_metadata, measure, run_logged,
        },
        parse_probe_symbols, source_manifest,
    },
    common::{sha256_file, write_json},
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, env, fmt::Write as _, fs, path::Path, process::Command};

const INLAY_ANCHOR: &str = "fn inlay_hints_warm(fixture: WarmSnapshot) -> Vec<analysis::InlayHint> {\n    std::hint::black_box(analysis::inlay_hints(\n        std::hint::black_box(fixture.snapshot),\n        analysis::TextRange::new(0, fixture.source.len()),\n    ))\n}";
const SETUP_ANCHORS: [&str; 3] = [
    "    fn small() -> Self {\n        Self {\n            source: SOURCE,\n            snapshot: &SMALL_SNAPSHOT,\n            completion_prefix: \"transform_\",\n            identifier_name: \"transform_31\",\n        }\n    }",
    "    fn medium() -> Self {\n        Self {\n            source: MEDIUM_SOURCE,\n            snapshot: &MEDIUM_SNAPSHOT,\n            completion_prefix: \"worker_\",\n            identifier_name: \"worker_0199\",\n        }\n    }",
    "    fn large() -> Self {\n        Self {\n            source: LARGE_SOURCE,\n            snapshot: &LARGE_SNAPSHOT,\n            completion_prefix: \"worker_\",\n            identifier_name: \"worker_0599\",\n        }\n    }",
];
const HELPERS: &str = "#[inline(never)]\nfn calibration_inlay_query(\n    snapshot: &analysis::DocumentSnapshot,\n    source_len: usize,\n) -> Vec<analysis::InlayHint> {\n    std::hint::black_box(analysis::inlay_hints(\n        std::hint::black_box(snapshot),\n        analysis::TextRange::new(0, source_len),\n    ))\n}\n\n#[inline(never)]\nfn calibration_allocation_control() {\n    std::hint::black_box(vec![1_u64; 65_536]);\n}\n\n";

fn exact_replace(text: &str, anchor: &str, replacement: &str) -> ToolResult<String> {
    require(
        text.matches(anchor).count() == 1,
        "calibration anchor must occur exactly once",
    )?;
    Ok(text.replacen(anchor, replacement, 1))
}

pub(super) fn variant_harness(text: &str, variant: &str) -> ToolResult<String> {
    require(VARIANTS.contains(&variant), "unknown calibration variant")?;
    // Rust source treats CRLF as LF; canonicalize before counting exact anchors so
    // checkout newlines cannot hide drift or mixed-newline duplicate anchors.
    let mut text = text.replace("\r\n", "\n");
    require(
        !text.contains(PROBE),
        "source harness already contains a calibration layout probe",
    )?;
    require(
        text.matches(INLAY_ANCHOR).count() == 1,
        "calibration anchor must occur exactly once",
    )?;
    for anchor in SETUP_ANCHORS {
        let replacement = anchor.replacen("        Self {", "        std::hint::black_box(calibration_layout_probe as fn(u64) -> u64);\n        std::hint::black_box(calibration_allocation_control as fn());\n        Self {", 1);
        text = exact_replace(&text, anchor, &replacement)?;
    }
    let (argument, body) = if variant == "layout" {
        let mut body = String::new();
        for index in 0..256_u64 {
            let constant = 0x9E37_79B9_7F4A_7C15_u64.wrapping_mul(index + 1);
            writeln!(
                body,
                "    state = state.rotate_left({}).wrapping_mul({}_u64) ^ {constant}_u64;",
                index % 63 + 1,
                constant | 1
            )?;
        }
        body.push_str("    state");
        ("mut state", body)
    } else {
        ("state", "    state".to_owned())
    };
    let helpers = format!(
        "#[inline(never)]\nfn {PROBE}({argument}: u64) -> u64 {{\n{body}\n}}\n\n{HELPERS}struct WarmSnapshot {{"
    );
    text = exact_replace(&text, "struct WarmSnapshot {", &helpers)?;
    let query = "calibration_inlay_query(\n        std::hint::black_box(fixture.snapshot),\n        std::hint::black_box(fixture.source.len()),\n    )";
    let measured = match variant {
        "extra_work" => {
            format!("    std::hint::black_box({query});\n    std::hint::black_box({query})")
        }
        "extra_alloc" => format!(
            "    let result = {query};\n    calibration_allocation_control();\n    std::hint::black_box(result)"
        ),
        _ => format!("    std::hint::black_box({query})"),
    };
    exact_replace(
        &text,
        INLAY_ANCHOR,
        &format!(
            "fn inlay_hints_warm(fixture: WarmSnapshot) -> Vec<analysis::InlayHint> {{\n{measured}\n}}"
        ),
    )
}

pub(super) fn balanced_order(job_id: &str, pair: u64) -> ToolResult<Vec<&'static str>> {
    require(pair > 0, "pair must be positive")?;
    let digest = Sha256::digest(job_id.as_bytes());
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    let seed = u64::from_be_bytes(bytes);
    // Modular addition avoids wrapping at high SHA256 seeds.
    let offset = usize::try_from((seed % 5 + (pair - 1) % 5) % 5)?;
    let mut order: Vec<_> = (0..5).map(|index| VARIANTS[(index + offset) % 5]).collect();
    if !(seed / 5 % 2 + (pair - 1) / 5 % 2).is_multiple_of(2) {
        order.reverse();
    }
    Ok(order)
}

fn independent_build(
    variant: &str,
    source: &Path,
    work: &Path,
    output: &Path,
    environment: &Environment,
    original: &Value,
    harness: &str,
) -> ToolResult<Build> {
    let checkout = work.join(variant);
    copy_verified_source(source, &checkout, original)?;
    fs::write(checkout.join("benches/analysis.rs"), harness)?;
    let mut build = compile(checkout, variant, output, environment)?;
    let analysis = build
        .benchmarks
        .get_mut("analysis")
        .ok_or("missing analysis executable")?;
    let mut nm = command_strings(&["nm", "-C", "-S", "--defined-only"]);
    nm.push(analysis.executable.as_os_str().to_owned());
    let symbols = run_logged(
        &nm,
        &build.checkout,
        &output.join(format!("builds/{variant}/analysis.nm.stdout")),
        &output.join(format!("builds/{variant}/analysis.nm.stderr")),
        environment,
    )?;
    analysis.metadata["layout_probe"] = parse_probe_symbols(&symbols, variant == "layout")?;
    build.metadata["benchmarks"]["analysis"] = analysis.metadata.clone();
    Ok(build)
}

fn record_provenance(source: &Path, output: &Path, dataset: &mut Value) -> ToolResult<Environment> {
    let environment = clean_environment();
    dataset["environment"] = environment_metadata(source, output, &environment)?;
    dataset["host"] = host_metadata()?;
    let revision = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["rev-parse", "HEAD"])
        .output()?;
    dataset["source_revision"] = if revision.status.success() {
        String::from_utf8(revision.stdout)?.trim().into()
    } else {
        "unknown".into()
    };
    let mut harness_hashes = serde_json::Map::new();
    for benchmark in BENCHMARKS {
        harness_hashes.insert(
            benchmark.to_owned(),
            sha256_file(&source.join(format!("benches/{benchmark}.rs")))?.into(),
        );
    }
    dataset["harness_sha256"] = harness_hashes.into();
    Ok(environment)
}

fn collect_job(
    args: &CollectArgs,
    source: &Path,
    work: &Path,
    output: &Path,
    dataset: &mut Value,
) -> ToolResult<()> {
    let environment = record_provenance(source, output, dataset)?;
    let excluded = [work.to_owned(), output.to_owned()];
    let original = source_manifest(source, &excluded)?;
    write_json(&output.join("source-manifest.json"), &original)?;
    let harness_path = source.join("benches/analysis.rs");
    let harness = fs::read_to_string(&harness_path)?;
    dataset["fixture_sha256"] =
        source_manifest(&source.join("benches/fixtures"), &excluded)?["sha256"].clone();
    let generated = VARIANTS
        .iter()
        .map(|variant| Ok((*variant, variant_harness(&harness, variant)?)))
        .collect::<ToolResult<BTreeMap<_, _>>>()?;
    let mut builds = BTreeMap::new();
    for variant in VARIANTS {
        let build = independent_build(
            variant,
            source,
            work,
            output,
            &environment,
            &original,
            &generated[variant],
        )?;
        if let Some(previous) = builds.values().next() {
            let previous: &Build = previous;
            require(
                build.identities == previous.identities,
                "independently built harness has a different executed workload manifest",
            )?;
            for benchmark in BENCHMARKS {
                require(
                    build.benchmarks[benchmark].identities
                        == previous.benchmarks[benchmark].identities,
                    "independently built benchmark has a different workload manifest",
                )?;
            }
        }
        dataset["variants"][variant] = build.metadata.clone();
        builds.insert(variant, build);
        write_json(&output.join("partial-data.json"), dataset)?;
    }
    require(
        dataset["variants"]["a"]["source_sha256"] == dataset["variants"]["b"]["source_sha256"],
        "A/A independent builds have different source manifests",
    )?;
    let full: shard::Selection = BENCHMARKS
        .iter()
        .map(|benchmark| {
            (
                *benchmark,
                builds["a"].benchmarks[*benchmark].identities.clone(),
            )
        })
        .collect();
    let selected = shard::selection(&full, args.shard_count, args.shard_index)?;
    require(
        shard::count(&selected) > 0,
        "shard selects no workloads from the frozen manifest",
    )?;
    let host = dataset["host"].clone();
    dataset["shard"] = shard_provenance(args, &full, &selected, &host)?;
    for pair in 1..=args.pairs {
        for (order, variant) in balanced_order(&args.job_id, pair)?.into_iter().enumerate() {
            let sample = measure(
                callgrind::collect::MeasurementScope {
                    build: &builds[variant],
                    selected: &selected,
                },
                &args.job_id,
                pair,
                variant,
                order,
                output,
                &environment,
            )?;
            dataset["samples"]
                .as_array_mut()
                .ok_or("samples must be an array")?
                .push(sample);
            write_json(&output.join("partial-data.json"), dataset)?;
        }
    }
    for variant in VARIANTS {
        ensure_immutable(&builds[variant])?;
        dataset["variants"][variant]["benchmarks"]["analysis"]["layout_probe_collected"] =
            false.into();
    }
    require(
        source_manifest(source, &excluded)? == original,
        "original source changed during calibration; job cannot be compared",
    )?;
    write_json(&output.join("data.json"), dataset)?;
    println!(
        "Collected {} complete pairs / {} fresh-process measurements in {}",
        args.pairs,
        dataset["samples"].as_array().map_or(0, Vec::len) * shard::count(&selected),
        output.join("data.json").display()
    );
    Ok(())
}

fn required_variable(key: &str) -> ToolResult<String> {
    let value =
        env::var(key).map_err(|error| format!("missing workflow environment {key}: {error}"))?;
    require(
        !value.trim().is_empty(),
        &format!("workflow environment {key} must be nonempty"),
    )?;
    Ok(value)
}

fn shard_provenance(
    args: &CollectArgs,
    full: &shard::Selection,
    selected: &shard::Selection,
    host: &Value,
) -> ToolResult<Value> {
    let logical = &args.job_id;
    let index = args.shard_index;
    let physical_job = if args.shard_count == 1 {
        logical.clone()
    } else {
        format!("{logical}-shard-{index}")
    };
    let matrix_job = required_variable("CALIBRATION_PHYSICAL_JOB")?;
    require(
        matrix_job == physical_job,
        "CALIBRATION_PHYSICAL_JOB must identify the launched physical shard job",
    )?;
    let runner = host["runner_identity"]
        .as_str()
        .filter(|runner| !runner.trim().is_empty() && *runner != "unknown")
        .ok_or("physical runner identity is missing")?
        .to_owned();
    Ok(json!({
        "method": shard::METHOD,
        "count": args.shard_count,
        "index": args.shard_index,
        "logical_job_id": args.job_id,
        "physical_job_id": physical_job,
        "matrix_job": matrix_job,
        "run_id": required_variable("GITHUB_RUN_ID")?,
        "run_attempt": required_variable("GITHUB_RUN_ATTEMPT")?,
        "physical_runner": runner,
        "full_manifest": shard::manifest(full)?,
        "selected_units": shard::units(selected),
    }))
}

pub(super) fn collect(args: &CollectArgs) -> ToolResult<()> {
    require(
        (1..=shard::MAX_COUNT).contains(&args.shard_count),
        "shard count must be between 1 and 4",
    )?;
    require(
        args.shard_index < args.shard_count,
        "shard index must be less than shard count",
    )?;
    let source = callgrind::resolve_path(&args.source)?;
    let work = callgrind::resolve_path(&args.work_dir)?;
    let output = callgrind::resolve_path(&args.output_dir)?;
    callgrind::validate_directories(&source, &work, &output)?;
    fs::create_dir_all(&work)?;
    fs::create_dir_all(&output)?;
    let mut dataset = json!({"format_version": 2, "job_id": args.job_id, "role": args.role, "variants": {}, "samples": []});
    if let Err(error) = collect_job(args, &source, &work, &output, &mut dataset) {
        write_json(&output.join("partial-data.json"), &dataset)?;
        write_json(
            &output.join("failure.json"),
            &json!({"error": error.to_string(), "type": "Error"}),
        )?;
        return Err(error);
    }
    Ok(())
}
