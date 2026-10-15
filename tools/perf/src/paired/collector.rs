use super::CollectArgs;
use crate::{
    ToolResult,
    callgrind::{self, BENCHMARKS, collect, require, source_manifest, strict_json},
    common::{native_target, write_json},
    policy::{ExitError, PROVENANCE_FIELDS},
    repeated::{METHOD, PAIRS},
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Source {
    variant: &'static str,
    path: PathBuf,
    revision: String,
    manifest: Value,
}

struct Collection {
    sources: [Source; 2],
    work: PathBuf,
    output: PathBuf,
    manifest: Value,
}

fn git(source: &Path, arguments: &[&str]) -> ToolResult<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(arguments)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "source Git verification failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn capture_source(
    path: PathBuf,
    expected: &str,
    variant: &'static str,
    excluded: &[PathBuf],
    output: &Path,
) -> ToolResult<Source> {
    let mut revision = git(&path, &["rev-parse", "HEAD"])?;
    revision.truncate(revision.trim_end().len());
    if revision != expected {
        write_json(
            &output.join("paired-failure.json"),
            &json!({
                "format_version": 1, "kind": "paired-callgrind-failure", "status": "error",
                "reason": {"kind": "source_revision_mismatch", "variant": variant, "expected": expected, "actual": revision}
            }),
        )?;
        return Err(format!("Source {variant} is {revision}, expected {expected}").into());
    }
    git(&path, &["diff", "--quiet", "HEAD", "--"])?;
    let manifest = source_manifest(&path, excluded)?;
    let tracked = git(&path, &["ls-files", "-z"])?;
    let tracked: BTreeSet<_> = tracked.split_terminator('\0').collect();
    for entry in manifest["files"].as_array().ok_or("missing source files")? {
        let name = entry["path"].as_str().ok_or("missing source path")?;
        if !tracked.contains(name) {
            return Err(format!("Source {variant} contains uncommitted input {name}").into());
        }
    }
    Ok(Source {
        variant,
        path,
        revision,
        manifest,
    })
}

fn prepare(args: &CollectArgs) -> ToolResult<Collection> {
    let output = args.output_dir.canonicalize()?;
    let work = callgrind::resolve_path(&args.work_dir)?;
    require(
        !work.starts_with(&output) && !output.starts_with(&work),
        "measurement work and evidence directories must be disjoint",
    )?;
    for name in [
        "paired-data.json",
        "paired-partial.json",
        "paired-failure.json",
        "raw",
        "manifests",
        "builds",
        "tools",
    ] {
        require(
            !output.join(name).exists(),
            "paired collection requires fresh evidence paths",
        )?;
    }
    let manifest = strict_json(&fs::read_to_string(output.join("artifact-manifest.json"))?)?;
    require(
        manifest["schema_version"] == 1
            && manifest["mode"] == "callgrind"
            && manifest["measurement_method"] == METHOD,
        "paired collection requires a prepared paired artifact envelope",
    )?;
    let baseline = args.baseline_source.canonicalize()?;
    let candidate = args.candidate_source.canonicalize()?;
    for source in [&baseline, &candidate] {
        require(
            !source.starts_with(&output),
            "evidence directory must not contain source",
        )?;
        callgrind::validate_directories(source, &work, &output.join("raw"))?;
    }
    let excluded = [work.clone(), output.clone()];
    let baseline = capture_source(
        baseline,
        manifest["base_sha"]
            .as_str()
            .ok_or("missing baseline revision")?,
        "a",
        &excluded,
        &output,
    )?;
    let candidate = capture_source(
        candidate,
        manifest["candidate_sha"]
            .as_str()
            .ok_or("missing candidate revision")?,
        "b",
        &excluded,
        &output,
    )?;
    Ok(Collection {
        sources: [baseline, candidate],
        work,
        output,
        manifest,
    })
}

fn dataset(collection: &Collection) -> ToolResult<Value> {
    let provenance: Map<String, Value> = PROVENANCE_FIELDS
        .into_iter()
        .map(|field| {
            let value = collection.manifest[field]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("missing {field}"))?;
            Ok((field.to_owned(), json!(value)))
        })
        .collect::<ToolResult<_>>()?;
    let job_id = format!(
        "{}:{}:{}:compare",
        provenance["repository"]
            .as_str()
            .ok_or("missing repository")?,
        provenance["run_id"].as_str().ok_or("missing run ID")?,
        provenance["run_attempt"]
            .as_str()
            .ok_or("missing run attempt")?
    );
    Ok(
        json!({"format_version": 1, "kind": "paired-callgrind", "method": METHOD, "pairs": PAIRS,
        "job_id": job_id, "provenance": provenance, "gate_identity": collection.manifest["gate_identity"],
        "variants": {}, "samples": []}),
    )
}

fn collect_job(collection: &Collection, data: &mut Value) -> ToolResult<()> {
    require(
        native_target()? == "x86_64-unknown-linux-gnu"
            && data["provenance"]["target"] == "x86_64-unknown-linux-gnu",
        "paired Callgrind admission requires the qualified Linux x86-64 target",
    )?;
    fs::create_dir_all(&collection.work)?;
    let environment = collect::clean_environment();
    for source in &collection.sources {
        let actual = collect::environment_metadata(
            &source.path,
            &collection.output.join("tools").join(source.variant),
            &environment,
        )?;
        if let Some(previous) = data.get("environment") {
            require(
                *previous == actual,
                "baseline and candidate require the same qualified collection tools",
            )?;
        } else {
            data["environment"] = actual;
        }
    }
    data["host"] = collect::host_metadata()?;
    super::report::metadata(data, &collection.manifest)?;
    let mut builds = std::collections::BTreeMap::new();
    for source in &collection.sources {
        let checkout = collection.work.join(source.variant);
        collect::copy_verified_source(&source.path, &checkout, &source.manifest)?;
        let mut build =
            collect::compile(checkout, source.variant, &collection.output, &environment)?;
        build.metadata["source_revision"] = source.revision.clone().into();
        data["variants"][source.variant] = build.metadata.clone();
        builds.insert(source.variant, build);
    }
    for benchmark in BENCHMARKS {
        require(
            builds["a"].benchmarks[benchmark]
                .identities
                .is_subset(&builds["b"].benchmarks[benchmark].identities),
            "candidate is missing a baseline benchmark workload",
        )?;
    }
    let job_id = data["job_id"]
        .as_str()
        .ok_or("missing measurement job ID")?
        .to_owned();
    for pair in 1..=u64::try_from(PAIRS)? {
        let order = if pair % 2 == 1 {
            ["a", "b"]
        } else {
            ["b", "a"]
        };
        for (position, variant) in order.into_iter().enumerate() {
            let sample = collect::measure(
                &builds[variant],
                &job_id,
                pair,
                variant,
                position,
                &collection.output,
                &environment,
            )?;
            data["samples"]
                .as_array_mut()
                .ok_or("missing sample array")?
                .push(sample);
            write_json(&collection.output.join("paired-partial.json"), data)?;
        }
    }
    let excluded = [collection.work.clone(), collection.output.clone()];
    for source in &collection.sources {
        collect::ensure_immutable(&builds[source.variant])?;
        require(
            source_manifest(&source.path, &excluded)? == source.manifest,
            "original source changed during measurement",
        )?;
        require(
            git(&source.path, &["rev-parse", "HEAD"])?.trim() == source.revision,
            "original source revision changed during measurement",
        )?;
        git(&source.path, &["diff", "--quiet", "HEAD", "--"])?;
    }
    write_json(&collection.output.join("paired-data.json"), data)?;
    println!(
        "Collected {PAIRS} complete source pairs across both benchmark executables in {}",
        collection.output.display()
    );
    Ok(())
}

pub(super) fn collect(args: &CollectArgs) -> ToolResult<()> {
    let collection = match prepare(args) {
        Ok(collection) => collection,
        Err(error) => return Err(ExitError::new(2, error.to_string()).into()),
    };
    let mut data = dataset(&collection)?;
    if let Err(error) = collect_job(&collection, &mut data) {
        write_json(&collection.output.join("paired-partial.json"), &data)?;
        write_json(
            &collection.output.join("paired-failure.json"),
            &json!({
                "format_version": 1, "kind": "paired-callgrind-failure", "status": "error",
                "reason": {"kind": "collection_failed", "error": error.to_string()}
            }),
        )?;
        return Err(error);
    }
    Ok(())
}
