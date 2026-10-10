use crate::{ToolResult, common};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

pub(super) const TARGETS: [&str; 6] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

#[derive(Serialize)]
pub(super) struct Report {
    pub format_version: u32,
    pub status: String,
    pub policy: &'static str,
    pub coverage_scope: &'static str,
    pub run: Value,
    pub issues: Vec<String>,
    pub errors: Vec<String>,
    pub targets: Vec<TargetEvidence>,
}

#[derive(Serialize)]
pub(super) struct TargetEvidence {
    pub target: String,
    pub status: String,
    pub provenance: Value,
    pub errors: Vec<String>,
    pub missing: Vec<String>,
    pub changed_scope: Vec<String>,
    pub regressions: Vec<String>,
    pub artifacts: Vec<Artifact>,
    pub documents: Vec<Document>,
}

#[derive(Serialize)]
pub(super) struct Artifact {
    pub path: String,
    pub kind: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Serialize)]
pub(super) struct Document {
    pub path: String,
    pub kind: String,
    pub status: String,
    pub data: Value,
    pub links: Vec<Artifact>,
}

impl Report {
    pub fn result(&self) -> ToolResult<()> {
        if matches!(self.status.as_str(), "failed" | "incomplete" | "regression") {
            return Err(format!("Performance report is {}; diagnostic index.html and unified-report.json were written", self.status).into());
        }
        Ok(())
    }
}

pub(super) fn confined(root: &Path, directory: &Path, relative: &str) -> ToolResult<PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || relative.contains('\\')
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("Artifact path is not a confined relative path: {relative}").into());
    }
    let path = directory.join(path).canonicalize()?;
    if !path.starts_with(root) {
        return Err(format!("Artifact escapes downloaded run: {relative}").into());
    }
    Ok(path)
}

fn read_json(path: &Path) -> ToolResult<Value> {
    Ok(serde_json::from_reader(fs::File::open(path)?)?)
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn relative(root: &Path, path: &Path) -> ToolResult<String> {
    Ok(path
        .strip_prefix(root)?
        .to_str()
        .ok_or("Non-Unicode artifact path")?
        .replace('\\', "/"))
}

fn scan(
    directory: &Path,
    depth: usize,
    manifests: &mut Vec<PathBuf>,
    profiles: &mut Vec<PathBuf>,
    requests: &mut Vec<PathBuf>,
) -> ToolResult<()> {
    if depth > 12 {
        return Err("Artifact directory nesting exceeds 12 levels".into());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            scan(&entry.path(), depth + 1, manifests, profiles, requests)?;
        } else if entry.file_name() == "artifact-manifest.json" {
            manifests.push(entry.path());
        } else if entry.file_name() == "profile-manifest.json" {
            profiles.push(entry.path());
        } else if entry.file_name() == "request.json" || entry.file_name() == "validation.json" {
            requests.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn collect(root: &Path) -> ToolResult<Report> {
    collect_scoped(root, false)
}

pub(super) fn collect_scoped(root: &Path, target_only: bool) -> ToolResult<Report> {
    let mut issues = Vec::new();
    let mut errors = Vec::new();
    let mut run = if root.join("run-manifest.json").exists() {
        match confined(root, root, "run-manifest.json").and_then(|path| read_json(&path)) {
            Ok(value) => value,
            Err(error) => {
                errors.push(format!("Invalid aggregate manifest: {error}"));
                Value::Null
            }
        }
    } else {
        Value::Null
    };
    let mut manifests = Vec::new();
    let mut profiles = Vec::new();
    let mut requests = Vec::new();
    scan(root, 0, &mut manifests, &mut profiles, &mut requests)?;
    let validation_failed = request_evidence(&requests, &mut run, &mut errors);
    manifests.sort();
    profiles.sort();
    let mut targets = load_targets(root, &manifests, &profiles, &mut run, &mut errors)?;
    validate_run(&run, &mut errors);
    if targets.is_empty() {
        issues.push("No artifact-manifest.json or profile-manifest.json found; expected measurements are absent".to_owned());
    }
    merge_callgrind(&mut targets, &mut errors);
    validate_coverage(&targets, &run, target_only, &mut issues, &mut errors);
    if run["workflow_succeeded"] == false {
        errors.push("Hosted workflow did not succeed; partial artifacts are retained, not accepted as a passing run".into());
    }
    let status = if validation_failed
        || !errors.is_empty()
        || targets.iter().any(|target| target.status == "failed")
    {
        "failed"
    } else if !issues.is_empty() || targets.iter().any(|target| target.status == "incomplete") {
        "incomplete"
    } else if targets.iter().any(|target| target.status == "regression") {
        "regression"
    } else if targets
        .iter()
        .any(|target| target.status == "changed-scope")
    {
        "changed-scope"
    } else {
        "complete"
    };
    Ok(Report {
        format_version: 1,
        coverage_scope: if target_only {
            "target-bundle; aggregate matrix coverage not evaluated"
        } else {
            "aggregate expected-target coverage"
        },
        status: status.into(),
        policy: "Hosted-only collection. Latency, resident memory and heap numbers are report-only. Callgrind gates remain authoritative. Never compare numerical results across native targets.",
        run,
        issues,
        errors,
        targets,
    })
}

fn load_target(root: &Path, path: &Path, manifest: &Value) -> ToolResult<TargetEvidence> {
    let directory = path.parent().ok_or("Manifest has no directory")?;
    let mut target = TargetEvidence {
        target: manifest["target"].as_str().unwrap_or("unknown").into(),
        status: "incomplete".into(),
        provenance: manifest.clone(),
        errors: Vec::new(),
        missing: Vec::new(),
        changed_scope: Vec::new(),
        regressions: Vec::new(),
        artifacts: Vec::new(),
        documents: Vec::new(),
    };
    if manifest["schema_version"] != 1 {
        target
            .errors
            .push("Unsupported artifact manifest schema_version".into());
    }
    if !TARGETS.contains(&target.target.as_str()) {
        target
            .errors
            .push("Missing or unsupported native target provenance".into());
    }
    for field in ["candidate_sha", "base_sha"] {
        if field == "base_sha" && manifest["mode"] == "profile" {
            continue;
        }
        if !manifest[field].as_str().is_some_and(|value| hex(value, 40)) {
            target
                .errors
                .push(format!("{field} must be an exact lowercase source SHA"));
        }
    }
    if manifest["mode"] != "profile" {
        for field in ["repository", "request_id"] {
            if manifest[field].as_str().is_none_or(str::is_empty) {
                target
                    .errors
                    .push(format!("Missing hosted {field} provenance"));
            }
        }
        if !manifest["workflow_sha"]
            .as_str()
            .is_some_and(|value| hex(value, 40))
        {
            target
                .errors
                .push("Missing or malformed exact workflow source SHA".into());
        }
    }
    let verified = verify_bundle(root, directory, manifest, &mut target);
    load_documents(root, directory, manifest, &verified, &mut target)?;
    pair_discovery(&mut target);
    target.status = if !target.errors.is_empty() {
        "failed"
    } else if !target.missing.is_empty() {
        "incomplete"
    } else if !target.regressions.is_empty() {
        "regression"
    } else if !target.changed_scope.is_empty() {
        "changed-scope"
    } else {
        "complete"
    }
    .into();
    Ok(target)
}

fn verify_artifact(
    root: &Path,
    directory: &Path,
    item: &Value,
    kind: &str,
) -> ToolResult<Artifact> {
    let name = item["artifact_path"]
        .as_str()
        .or_else(|| item["path"].as_str())
        .ok_or("Artifact identity has no path")?;
    let path = confined(root, directory, name)?;
    let hash = item["sha256"]
        .as_str()
        .ok_or("Artifact identity has no SHA256")?;
    if !hex(hash, 64) || common::sha256_file(&path)? != hash {
        return Err(format!("Artifact SHA256 mismatch: {name}").into());
    }
    let size = fs::metadata(&path)?.len();
    if item["size"]
        .as_u64()
        .or_else(|| item["bytes"].as_u64())
        .is_some_and(|expected| expected != size)
    {
        return Err(format!("Artifact size mismatch: {name}").into());
    }
    Ok(Artifact {
        path: relative(root, &path)?,
        kind: kind.into(),
        sha256: hash.into(),
        size,
    })
}

fn require_hash(value: &Value, context: &str, target: &mut TargetEvidence) {
    if !value.as_str().is_some_and(|value| hex(value, 64)) {
        target
            .errors
            .push(format!("Missing or malformed binary SHA256: {context}"));
    }
}

fn check_pair(metadata: &Value, target: &mut TargetEvidence) {
    let native = metadata["native_target"]
        .as_str()
        .or_else(|| metadata["target"].as_str());
    if native != Some(target.target.as_str()) {
        target
            .errors
            .push("Collector native target differs from artifact provenance".into());
    }
    for (field, expected) in [
        ("baseline_revision", "base_sha"),
        ("candidate_revision", "candidate_sha"),
    ] {
        if metadata[field] != target.provenance[expected] {
            target
                .errors
                .push(format!("Collector {field} differs from source provenance"));
        }
    }
}

fn validate_document(
    root: &Path,
    directory: &Path,
    document: &mut Document,
    target: &mut TargetEvidence,
) {
    let data = &document.data;
    let status = data["status"]
        .as_str()
        .unwrap_or(if document.kind == "latency" {
            "complete"
        } else {
            "unknown"
        });
    document.status = status.into();
    if status == "failed" {
        target
            .errors
            .push(format!("{} failed: {}", document.path, data["error"]));
    } else if status != "complete" && document.kind != "callgrind" {
        target
            .missing
            .push(format!("{} collection status: {status}", document.path));
    }
    if document.kind != "callgrind" && data["format_version"] != 1 {
        target
            .errors
            .push(format!("{} unsupported format_version", document.path));
    }
    match document.kind.as_str() {
        "latency" => validate_latency(data, target),
        "discovery" | "discovery-raw" => {
            validate_discovery(data, document.kind == "discovery", target);
        }
        "memory" => validate_heap(root, directory, document, target),
        "profile" => validate_profile(root, directory, document, target),
        // Do not redefine or weaken existing Callgrind policy: render its own gate verdict.
        "callgrind"
            if data["status"] == "regression"
                || data["gate_status"] == "regression"
                || data["passed"] == false =>
        {
            target
                .regressions
                .push("Callgrind gate reported a regression; see original gate evidence".into());
        }
        _ => {}
    }
}
fn validate_latency(data: &Value, target: &mut TargetEvidence) {
    for variant in ["baseline", "candidate"] {
        let metadata = &data[format!("{variant}_metadata")];
        if metadata["target"] != target.target {
            target
                .errors
                .push(format!("Latency {variant} target mismatch"));
        }
        require_hash(&metadata["binary_sha256"], variant, target);
        if metadata["collection_status"] != "complete" {
            target
                .missing
                .push(format!("Latency {variant} collection not complete"));
        }
        let revision = if variant == "baseline" {
            "base_sha"
        } else {
            "candidate_sha"
        };
        if !metadata["source_sha"].is_null()
            && metadata["source_sha"] != target.provenance[revision]
        {
            target
                .errors
                .push(format!("Latency {variant} source mismatch"));
        }
    }
    let left = &data["baseline_metadata"];
    let right = &data["candidate_metadata"];
    for field in [
        "target",
        "platform",
        "machine",
        "harness_sha256",
        "fixture_sha256",
        "rounds",
        "samples",
        "warmup",
    ] {
        if left[field].is_null() || left[field] != right[field] {
            target
                .errors
                .push(format!("Latency paired {field} differs or is absent"));
        }
    }
    if !data["workload_digest"]
        .as_str()
        .is_some_and(|value| hex(value, 64))
    {
        target
            .errors
            .push("Latency workload digest is absent or malformed".into());
    }
    let names = [
        "hover_warm",
        "definition_warm",
        "completion_warm",
        "open_to_hover_large",
        "edit_to_hover_large",
        "hover_during_large_edit",
        "hover_during_large_open",
    ];
    for name in names {
        for variant in ["baseline", "candidate"] {
            let rounds = data["workloads"][name][variant]["rounds"].as_array();
            if rounds.is_none_or(|rounds| {
                u64::try_from(rounds.len()).ok() != left["rounds"].as_u64()
                    || rounds.is_empty()
                    || rounds.iter().any(|round| {
                        round["p50_ns"].as_u64().is_none() || round["p95_ns"].as_u64().is_none()
                    })
            }) {
                target.missing.push(format!(
                    "Latency {name}/{variant} round distributions incomplete"
                ));
            }
        }
    }
}

fn pair_discovery(target: &mut TargetEvidence) {
    let report = target
        .documents
        .iter()
        .find(|document| document.kind == "discovery");
    let raw = target
        .documents
        .iter()
        .find(|document| document.kind == "discovery-raw");
    if let Some(report) = report {
        if let Some(raw) = raw {
            if report.data["metadata"] != raw.data["metadata"]
                || report.data["status"] != raw.data["status"]
            {
                target
                    .errors
                    .push("Discovery raw/report provenance or collection status differ".into());
            }
        } else {
            target.missing.push(
                "Discovery raw evidence absent; timeline/source pairing cannot be verified".into(),
            );
        }
    }
}

fn request_evidence(paths: &[PathBuf], run: &mut Value, issues: &mut Vec<String>) -> bool {
    let mut failed = false;
    for path in paths {
        let value = match read_json(path) {
            Ok(value) => value,
            Err(error) => {
                issues.push(format!("Invalid request/validation evidence: {error}"));
                failed = true;
                continue;
            }
        };
        if path
            .file_name()
            .is_some_and(|name| name == "validation.json")
        {
            if value["status"] != "success" {
                issues.push(format!(
                    "Hosted request validation failed: {}",
                    value["error"]
                ));
                failed = true;
            }
            continue;
        }
        if run.is_null() {
            let expected = if value["SELECTED_TARGET"] == "all" {
                json!(TARGETS)
            } else {
                json!([value["SELECTED_TARGET"]])
            };
            *run = json!({"format_version":1,"mode":value["MODE"],"request_id":value["REQUEST_ID"],
                "base_sha":value["BASE_SHA"],"candidate_sha":value["CANDIDATE_SHA"],"repository":value["repository"],
                "expected_targets":expected,"request":value});
        } else {
            for (field, request_field) in [
                ("request_id", "REQUEST_ID"),
                ("base_sha", "BASE_SHA"),
                ("candidate_sha", "CANDIDATE_SHA"),
                ("mode", "MODE"),
            ] {
                if run[field] != value[request_field] {
                    issues.push(format!(
                        "Downloaded request {request_field} differs from run provenance"
                    ));
                    failed = true;
                }
            }
        }
    }
    failed
}

fn merge_callgrind(targets: &mut Vec<TargetEvidence>, issues: &mut Vec<String>) {
    let mut index = 0;
    while index < targets.len() {
        if targets[index].provenance["mode"] != "callgrind" {
            index += 1;
            continue;
        }
        let destination = targets.iter().position(|target| {
            target.target == targets[index].target && target.provenance["mode"] != "callgrind"
        });
        let Some(destination) = destination else {
            index += 1;
            continue;
        };
        let gate = targets.remove(index);
        let destination = if destination > index {
            destination - 1
        } else {
            destination
        };
        let target = &mut targets[destination];
        for field in ["base_sha", "candidate_sha", "repository", "request_id"] {
            if gate.provenance[field] != target.provenance[field] {
                issues.push(format!(
                    "Callgrind {field} differs from native target provenance"
                ));
            }
        }
        target.provenance["callgrind_manifest"] = gate.provenance;
        target.errors.extend(gate.errors);
        target.missing.extend(gate.missing);
        target.regressions.extend(gate.regressions);
        target.artifacts.extend(gate.artifacts);
        target.documents.extend(gate.documents);
        target.status = if !target.errors.is_empty() {
            "failed"
        } else if !target.missing.is_empty() {
            "incomplete"
        } else if !target.regressions.is_empty() {
            "regression"
        } else if !target.changed_scope.is_empty() {
            "changed-scope"
        } else {
            "complete"
        }
        .into();
    }
}

fn validate_discovery(data: &Value, summary: bool, target: &mut TargetEvidence) {
    check_pair(&data["metadata"], target);
    for variant in ["baseline", "candidate"] {
        require_hash(
            &data["metadata"]["binaries"][variant]["sha256"],
            variant,
            target,
        );
    }
    let counts = data["metadata"]["dataset_counts"].as_array();
    if counts.is_none_or(Vec::is_empty) {
        target
            .missing
            .push("Discovery expected datasets absent".into());
    }
    for count in counts.into_iter().flatten() {
        let section = &data["datasets"][count.to_string()];
        if section.is_null() {
            target
                .missing
                .push(format!("Discovery dataset {count} absent"));
            continue;
        }
        if !summary {
            continue;
        }
        for variant in ["baseline", "candidate"] {
            if section[variant]["completed_rounds"] != data["metadata"]["rounds"] {
                target
                    .missing
                    .push(format!("Discovery {count}/{variant} rounds incomplete"));
            }
            validate_process_memory(
                &section[variant],
                &format!("Discovery {count}/{variant}"),
                &data["metadata"]["rounds"],
                target,
            );
            for observation in section[variant]["semantic_observations"]
                .as_array()
                .into_iter()
                .flatten()
            {
                for name in [
                    "cold_symbols",
                    "cold_references",
                    "symbols_after_open",
                    "references_after_open",
                ] {
                    let scope = &observation[name];
                    if variant == "baseline" && !scope.is_null() && scope["complete"] != true {
                        target.changed_scope.push(format!("Discovery {count}/{name}: baseline {}/{}; changed scope, not comparable latency", scope["observed"], scope["expected"]));
                    }
                    if variant == "candidate" && !scope.is_null() && scope["complete"] != true {
                        target.errors.push(format!(
                            "Discovery {count}/{name}: candidate semantic answer is incomplete"
                        ));
                    }
                }
            }
        }
    }
}

fn validate_process_memory(
    variant: &Value,
    context: &str,
    expected_rounds: &Value,
    target: &mut TargetEvidence,
) {
    let Some(observations) = variant["memory_observations"].as_array() else {
        target.missing.push(format!(
            "{context}: required resident-memory observations absent"
        ));
        return;
    };
    let completed = observations
        .iter()
        .filter(|observation| observation["status"] == "complete")
        .count();
    if expected_rounds.as_u64() != u64::try_from(completed).ok() {
        target
            .missing
            .push(format!("{context}: resident-memory rounds incomplete"));
    }
    for observation in observations
        .iter()
        .filter(|observation| observation["status"] == "complete")
    {
        let missing_before = target.missing.len();
        if !observation["samples"].as_array().is_some_and(|samples| {
            samples.iter().any(|sample| {
                sample["rss_bytes"].as_u64().is_some() && sample["elapsed_ns"].as_u64().is_some()
            })
        }) {
            target.missing.push(format!(
                "{context}/round {}: resident timeline unavailable",
                observation["round"]
            ));
        }
        for name in [
            "initialize_response",
            "initial_completion_response",
            "cold_discovery_observed",
            "before_root_removal",
            "after_root_removal",
            "after_root_removal_settled",
        ] {
            let checkpoint = &observation["checkpoints"][name];
            if checkpoint["rss_bytes"].as_u64().is_none()
                || checkpoint["elapsed_ns"].as_u64().is_none()
            {
                target.missing.push(format!(
                    "{context}/round {}: required resident checkpoint {name} unavailable",
                    observation["round"]
                ));
            }
        }
        if target.missing.len() != missing_before {
            for error in observation["errors"].as_array().into_iter().flatten() {
                target.missing.push(format!(
                    "{context}/round {}: memory observer: {error}",
                    observation["round"]
                ));
            }
        }
    }
}

fn validate_heap(
    root: &Path,
    directory: &Path,
    document: &mut Document,
    target: &mut TargetEvidence,
) {
    let data = &document.data;
    if data["native_target"] != target.target {
        target.errors.push("DHAT target provenance mismatch".into());
    }
    for (field, expected) in [
        ("baseline_revision", "base_sha"),
        ("candidate_revision", "candidate_sha"),
    ] {
        if data["provenance"]["source"][field] != target.provenance[expected] {
            target
                .errors
                .push(format!("DHAT {field} differs from source provenance"));
        }
    }
    let profiles = data["profiles"].as_array();
    if profiles.is_none_or(|profiles| profiles.len() != 13) {
        target
            .missing
            .push("Expected all 13 DHAT scenarios; coverage incomplete".into());
    }
    let mut names = BTreeSet::new();
    for profile in profiles.into_iter().flatten() {
        if !names.insert(profile["name"].as_str().unwrap_or_default()) {
            target.errors.push("Duplicate DHAT scenario".into());
        }
        if profile["status"] != "complete" {
            target.errors.push(format!(
                "DHAT {}: {} {}",
                profile["name"], profile["status"], profile["error"]
            ));
            continue;
        }
        for field in [
            "total_allocated_bytes",
            "total_allocated_blocks",
            "global_peak_live_bytes",
            "global_peak_live_blocks",
            "end_live_bytes",
            "end_live_blocks",
        ] {
            if profile[field].as_u64().is_none() {
                target
                    .errors
                    .push(format!("DHAT {} missing {field}", profile["name"]));
            }
        }
        // Legacy collectors record an absolute hosted path; stable scenario-relative paths survive download.
        let name = profile["name"].as_str().unwrap_or_default();
        let item =
            json!({"path":format!("{name}/dhat-heap.json"), "sha256":profile["profile"]["sha256"]});
        match verify_artifact(root, directory, &item, "dhat") {
            Ok(artifact) => document.links.push(artifact),
            Err(error) => target.errors.push(error.to_string()),
        }
    }
}

fn validate_profile(
    root: &Path,
    directory: &Path,
    document: &mut Document,
    target: &mut TargetEvidence,
) {
    let data = &document.data;
    if data["target"] != target.target || data["source_sha"] != target.provenance["candidate_sha"] {
        target
            .errors
            .push("Profile source/target provenance mismatch".into());
    }
    require_hash(&data["binary"]["sha256"], "profile", target);
    if data["status"] == "complete" {
        let result = &data["scenario_result"];
        if result["status"] != "complete" || result["scenario"] != data["scenario"] {
            target
                .errors
                .push("Profile has no matching complete semantic scenario result".into());
        }
        if result["binary_sha256"] != data["binary"]["sha256"]
            || result["pid"].as_u64().is_none()
            || result["pid"] != data["target_pid"]
        {
            target.errors.push(
                "Profile scenario process/binary identity differs from captured process".into(),
            );
        }
        if result["semantics"]["shutdown"]["clean_exit"] != true {
            target
                .errors
                .push("Profile child did not record a semantically checked clean shutdown".into());
        }
    }
    for identity in std::iter::once(&data["binary"])
        .chain(data["binary"]["symbols"].as_array().into_iter().flatten())
    {
        match verify_artifact(root, directory, identity, "binary/symbol") {
            Ok(artifact) => document.links.push(artifact),
            Err(error) => target
                .errors
                .push(format!("Profile binary/symbol identity: {error}")),
        }
    }
    let artifacts = data["artifacts"].as_array();
    if artifacts.is_none_or(Vec::is_empty) {
        target.missing.push("Profile trace artifacts absent".into());
    }
    for item in artifacts.into_iter().flatten() {
        match verify_artifact(
            root,
            directory,
            item,
            item["kind"].as_str().unwrap_or("trace"),
        ) {
            Ok(artifact) => document.links.push(artifact),
            Err(error) => target.errors.push(error.to_string()),
        }
    }
    if let Some(errors) = data["errors"].as_array() {
        target
            .errors
            .extend(errors.iter().map(|error| format!("Profile error: {error}")));
    }
}

fn validate_coverage(
    targets: &[TargetEvidence],
    run: &Value,
    target_only: bool,
    issues: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    let mut seen = BTreeSet::new();
    for target in targets {
        if !seen.insert(target.target.clone()) {
            errors.push(format!(
                "Duplicate artifact bundles for target {}",
                target.target
            ));
        }
        for field in [
            "base_sha",
            "candidate_sha",
            "repository",
            "request_id",
            "mode",
        ] {
            if field == "mode" && target.provenance["mode"] == "callgrind" {
                continue;
            }
            if !run[field].is_null() && run[field] != target.provenance[field] {
                errors.push(format!(
                    "{} {field} does not match aggregate provenance",
                    target.target
                ));
            }
        }
    }
    let Some(expected) = run["expected_targets"].as_array().filter(|_| !target_only) else {
        return;
    };
    let mut expected_seen = BTreeSet::new();
    for value in expected {
        let name = value.as_str().unwrap_or("invalid target");
        if !TARGETS.contains(&name) || !expected_seen.insert(name) {
            errors.push(format!("Invalid or duplicate expected target: {name}"));
        }
        if !seen.contains(name) {
            issues.push(format!("Expected target artifact is absent: {name}"));
        }
    }
    for name in &seen {
        if !expected_seen.contains(name.as_str()) {
            errors.push(format!("Unexpected target artifact: {name}"));
        }
    }
}

fn verify_bundle(
    root: &Path,
    directory: &Path,
    manifest: &Value,
    target: &mut TargetEvidence,
) -> BTreeSet<String> {
    let mut verified = BTreeSet::new();
    if let Some(files) = manifest["files"].as_array() {
        for item in files {
            match verify_artifact(root, directory, item, "artifact") {
                Ok(artifact) => {
                    if !verified.insert(item["path"].as_str().unwrap_or_default().to_owned()) {
                        target
                            .errors
                            .push("Duplicate file identity in manifest".into());
                    }
                    target.artifacts.push(artifact);
                }
                Err(error) => target.errors.push(error.to_string()),
            }
        }
    } else {
        target
            .missing
            .push("Manifest file identities are absent".into());
    }
    if let Some(statuses) = manifest["statuses"].as_object() {
        for (name, value) in statuses {
            let status = value
                .as_str()
                .or_else(|| value["status"].as_str())
                .unwrap_or("unknown");
            match status {
                "complete" | "passed" | "success" | "skipped" | "not-applicable" => {}
                "regression" => target.regressions.push(format!("{name}: {value}")),
                "failed" | "failure" | "error" => target.errors.push(format!("{name}: {value}")),
                _ => target.missing.push(format!("{name}: {value}")),
            }
        }
    }
    verified
}

fn load_documents(
    root: &Path,
    directory: &Path,
    manifest: &Value,
    verified: &BTreeSet<String>,
    target: &mut TargetEvidence,
) -> ToolResult<()> {
    let Some(expected) = manifest["expected"].as_array() else {
        target
            .missing
            .push("Manifest has no expected collector coverage".into());
        return Ok(());
    };
    if expected.is_empty() {
        target
            .missing
            .push("Expected collector coverage is empty".into());
    }
    let mut expected_seen = BTreeSet::new();
    for item in expected {
        let name = item["path"].as_str().unwrap_or_default();
        let kind = item["kind"].as_str().unwrap_or("artifact");
        if !expected_seen.insert(name) {
            target
                .errors
                .push(format!("Duplicate expected artifact: {name}"));
            continue;
        }
        let artifact_path = match confined(root, directory, name) {
            Ok(path) => path,
            Err(error) => {
                if item["required"] != false {
                    target.missing.push(format!("{kind}: {error}"));
                }
                continue;
            }
        };
        if !verified.contains(name) {
            target.errors.push(format!(
                "Artifact has no verified manifest identity: {name}"
            ));
            continue;
        }
        if !matches!(
            kind,
            "latency" | "discovery" | "discovery-raw" | "memory" | "profile" | "callgrind"
        ) {
            continue;
        }
        if !name.ends_with(".json") {
            target.errors.push(format!(
                "Collector report must have a JSON filename: {name}"
            ));
            continue;
        }
        let data = match read_json(&artifact_path) {
            Ok(data) => data,
            Err(error) => {
                target.errors.push(format!("{name}: invalid JSON: {error}"));
                continue;
            }
        };
        let mut document = Document {
            path: relative(root, &artifact_path)?,
            kind: kind.into(),
            status: "complete".into(),
            data,
            links: Vec::new(),
        };
        if kind == "profile"
            && name.starts_with("profiles/")
            && name.split('/').nth(1) != document.data["scenario"].as_str()
        {
            target.errors.push(format!(
                "Profile scenario differs from expected artifact scope: {name}"
            ));
        }
        validate_document(
            root,
            artifact_path.parent().ok_or("Missing artifact directory")?,
            &mut document,
            target,
        );
        target.documents.push(document);
    }
    Ok(())
}

fn validate_run(run: &Value, issues: &mut Vec<String>) {
    if run.is_null() {
        return;
    }
    if run["format_version"] != 1 {
        issues.push("Unsupported aggregate run manifest version".to_owned());
    }
    for field in ["base_sha", "candidate_sha"] {
        if !run[field].as_str().is_some_and(|value| hex(value, 40)) {
            issues.push(format!(
                "Aggregate {field} is not an exact lowercase source SHA"
            ));
        }
    }
    if run["expected_targets"].as_array().is_none_or(Vec::is_empty) {
        issues.push("Aggregate manifest has no expected target coverage".to_owned());
    }
}

fn load_targets(
    root: &Path,
    manifests: &[PathBuf],
    profiles: &[PathBuf],
    run: &mut Value,
    issues: &mut Vec<String>,
) -> ToolResult<Vec<TargetEvidence>> {
    let mut targets = Vec::new();
    for path in manifests {
        match read_json(path) {
            Ok(manifest) => {
                if run.is_null() || (run["mode"] == "callgrind" && manifest["mode"] != "callgrind")
                {
                    *run = json!({"format_version":1,"mode":manifest["mode"],"base_sha":manifest["base_sha"],
                        "candidate_sha":manifest["candidate_sha"],"repository":manifest["repository"],
                        "request_id":manifest["request_id"],"expected_targets":manifest["expected_targets"]});
                }
                targets.push(load_target(root, path, &manifest)?);
            }
            Err(error) => issues.push(format!(
                "Invalid artifact manifest {}: {error}",
                relative(root, path)?
            )),
        }
    }
    // A downloaded single profile can be read without a comparison manifest.
    if targets.is_empty() {
        for path in profiles {
            let data = match read_json(path) {
                Ok(value) => value,
                Err(error) => {
                    issues.push(format!("Invalid profile manifest: {error}"));
                    continue;
                }
            };
            let manifest = json!({"schema_version":1, "mode":"profile", "target":data["target"],
                "candidate_sha":data["source_sha"], "expected":[{"path":"profile-manifest.json","kind":"profile","required":true}],
                "files":[{"path":"profile-manifest.json", "sha256":common::sha256_file(path)?, "size":fs::metadata(path)?.len()}]});
            targets.push(load_target(root, path, &manifest)?);
        }
    }
    Ok(targets)
}
