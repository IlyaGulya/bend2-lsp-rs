//! Hosted workflow source validation and failure-aware artifact inventories.
use crate::{ToolResult, common};
use serde_json::{Map, Value, json};
use std::{
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

struct Target {
    runner: &'static str,
    arch: &'static str,
    triple: &'static str,
}

static TARGETS: [Target; 6] = [
    Target {
        runner: "ubuntu-24.04",
        arch: "X64",
        triple: "x86_64-unknown-linux-gnu",
    },
    Target {
        runner: "ubuntu-24.04-arm",
        arch: "ARM64",
        triple: "aarch64-unknown-linux-gnu",
    },
    Target {
        runner: "macos-15-intel",
        arch: "X64",
        triple: "x86_64-apple-darwin",
    },
    Target {
        runner: "macos-15",
        arch: "ARM64",
        triple: "aarch64-apple-darwin",
    },
    Target {
        runner: "windows-2022",
        arch: "X64",
        triple: "x86_64-pc-windows-msvc",
    },
    Target {
        runner: "windows-11-arm",
        arch: "ARM64",
        triple: "aarch64-pc-windows-msvc",
    },
];
const SCENARIOS: [&str; 4] = [
    "discovery-10",
    "discovery-1000",
    "discovery-10000",
    "latency",
];

fn variable(key: &str) -> ToolResult<String> {
    env::var(key).map_err(|error| format!("Missing workflow environment {key}: {error}").into())
}

struct CollectionRequest {
    request_id: String,
    mode: String,
    base_sha: String,
    candidate_sha: String,
    scenario: String,
    selected_target: String,
    native_kind: String,
}

impl CollectionRequest {
    fn read() -> ToolResult<Self> {
        Ok(Self {
            request_id: variable("REQUEST_ID")?,
            mode: variable("MODE")?,
            base_sha: variable("BASE_SHA")?,
            candidate_sha: variable("CANDIDATE_SHA")?,
            scenario: variable("SCENARIO")?,
            selected_target: variable("SELECTED_TARGET")?,
            native_kind: env::var("NATIVE_KIND").unwrap_or_else(|_| "cpu".to_owned()),
        })
    }

    fn targets(&self) -> impl Iterator<Item = &'static Target> + '_ {
        TARGETS.iter().filter(move |target| {
            self.selected_target == "all" || self.selected_target == target.triple
        })
    }
}

fn git(arguments: &[&str]) -> ToolResult<String> {
    let output = Command::new("git").args(arguments).output()?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let mut text = String::from_utf8(output.stdout)?;
    text.truncate(text.trim_end().len());
    Ok(text)
}

fn validate_source(request: &CollectionRequest) -> ToolResult<()> {
    let id = request.request_id.as_bytes();
    if !(8..=96).contains(&id.len())
        || !id
            .first()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !id
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    {
        return Err(
            "request_id must be a unique 8-96 character lowercase alphanumeric/hyphen ID".into(),
        );
    }
    for (name, sha) in [
        ("BASE_SHA", &request.base_sha),
        ("CANDIDATE_SHA", &request.candidate_sha),
    ] {
        if sha.len() != 40
            || !sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(
                format!("{name} must be an exact lowercase 40-character source SHA").into(),
            );
        }
    }
    let event = variable("GITHUB_EVENT_NAME")?;
    if !(matches!(
        request.mode.as_str(),
        "compare" | "cpu" | "heap" | "native" | "full"
    ) || (event == "pull_request" && request.mode == "verification"))
        || !SCENARIOS.contains(&request.scenario.as_str())
    {
        return Err("Unknown mode or scenario".into());
    }
    if request.targets().next().is_none() {
        return Err("Requested platform is unsupported; refusing to silently skip it".into());
    }
    if !matches!(request.native_kind.as_str(), "cpu" | "heap") {
        return Err("native_kind must be cpu or heap".into());
    }
    if request.mode == "native"
        && request.native_kind == "heap"
        && request
            .targets()
            .any(|target| target.triple.contains("linux"))
    {
        return Err(
            "Linux native heap tracing is unsupported: use DHAT heap mode or select macOS/Windows"
                .into(),
        );
    }
    if git(&["rev-parse", "HEAD"])? != variable("WORKFLOW_SHA")? {
        return Err("Tooling checkout differs from the immutable workflow source SHA".into());
    }
    let origin = git(&["remote", "get-url", "origin"])?;
    let server = env::var("GITHUB_SERVER_URL").unwrap_or_else(|_| "https://github.com".to_owned());
    if origin.strip_suffix(".git").unwrap_or(&origin)
        != format!("{server}/{}", variable("GITHUB_REPOSITORY")?)
    {
        return Err("Source repository identity differs from workflow repository".into());
    }
    if event != "pull_request" {
        let reference = variable("GITHUB_REF")?;
        if !(reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/")) {
            return Err("Dispatch must use a published repository branch or tag".into());
        }
        git(&["check-ref-format", &reference])?;
        git(&[
            "fetch",
            "--no-tags",
            "origin",
            &format!("+{reference}:refs/perf/workflow-source"),
        ])?;
        if git(&["rev-parse", "refs/perf/workflow-source^{commit}"])? != variable("WORKFLOW_SHA")? {
            return Err(
                "Published workflow ref moved; redispatch against its new exact source SHA".into(),
            );
        }
    }
    git(&[
        "fetch",
        "--no-tags",
        "origin",
        "+refs/heads/*:refs/remotes/origin/*",
        "+refs/tags/*:refs/tags/*",
    ])?;
    let pull_ref = if event == "pull_request" {
        let number = variable("PULL_NUMBER")?;
        if !number
            .as_bytes()
            .first()
            .is_some_and(|byte| (b'1'..=b'9').contains(byte))
            || !number.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err("Missing pull request identity".into());
        }
        let reference = "refs/perf/pull-source";
        git(&[
            "fetch",
            "--no-tags",
            "origin",
            &format!("refs/pull/{number}/merge:{reference}"),
        ])?;
        if git(&["rev-parse", reference])? != request.candidate_sha {
            return Err("Pull request source moved; redispatch on the new exact SHA".into());
        }
        Some(reference)
    } else {
        None
    };
    for (name, sha) in [
        ("BASE_SHA", &request.base_sha),
        ("CANDIDATE_SHA", &request.candidate_sha),
    ] {
        if git(&["rev-parse", &format!("{sha}^{{commit}}")])? != *sha {
            return Err(format!("{name} does not resolve to the exact requested commit").into());
        }
        let contains = format!("--contains={sha}");
        let mut arguments = vec![
            "for-each-ref",
            &contains,
            "--format=%(refname)",
            "refs/remotes/origin",
            "refs/tags",
        ];
        if let Some(reference) = pull_ref {
            arguments.push(reference);
        }
        if git(&arguments)?.is_empty() {
            return Err(format!(
                "{name} is not reachable from this repository's published source refs"
            )
            .into());
        }
    }
    let matrix: Vec<Value> = request
        .targets()
        .map(
            |target| json!({"runner": target.runner, "arch": target.arch, "target": target.triple}),
        )
        .collect();
    let mut output = OpenOptions::new()
        .append(true)
        .open(variable("GITHUB_OUTPUT")?)?;
    writeln!(
        output,
        "matrix={}",
        serde_json::to_string(&json!({"include": matrix}))?
    )?;
    for (name, value) in [
        ("request_id", &request.request_id),
        ("mode", &request.mode),
        ("base_sha", &request.base_sha),
        ("candidate_sha", &request.candidate_sha),
        ("scenario", &request.scenario),
        ("selected_target", &request.selected_target),
        ("native_kind", &request.native_kind),
    ] {
        writeln!(output, "{name}={value}")?;
    }
    Ok(())
}

fn validate() -> ToolResult<()> {
    let root = Path::new("target/perf-request");
    let outcome = (|| -> ToolResult<()> {
        let request = CollectionRequest::read()?;
        common::write_json(
            &root.join("request.json"),
            &json!({
                "REQUEST_ID": request.request_id, "MODE": request.mode,
                "BASE_SHA": request.base_sha, "CANDIDATE_SHA": request.candidate_sha,
                "SCENARIO": request.scenario, "SELECTED_TARGET": request.selected_target,
                "NATIVE_KIND": request.native_kind, "repository": variable("GITHUB_REPOSITORY")?,
                "workflow_sha": variable("WORKFLOW_SHA")?, "event": variable("GITHUB_EVENT_NAME")?,
            }),
        )?;
        validate_source(&request)
    })();
    let result = match &outcome {
        Ok(()) => json!({"status": "success"}),
        Err(error) => json!({"status": "failure", "error": error.to_string()}),
    };
    common::write_json(&root.join("validation.json"), &result)?;
    outcome
}

fn expected(request: &CollectionRequest, target: &str) -> Vec<Value> {
    let mut result = Vec::new();
    let mut add = |path: String, kind: &str| {
        result.push(json!({"path": path, "kind": kind, "required": true}));
    };
    if request.mode == "callgrind" {
        add("analysis_hot_paths-baseline.json".to_owned(), "callgrind");
    }
    if matches!(request.mode.as_str(), "compare" | "full") {
        for (path, kind) in [
            ("latency-report.json", "latency"),
            ("discovery/report.json", "discovery"),
            ("discovery/raw.json", "discovery-raw"),
            ("provenance.json", "provenance"),
        ] {
            add(path.to_owned(), kind);
        }
    }
    if request.mode == "full" {
        add("memory/report.json".to_owned(), "memory");
        add("memory/lsp-lifecycle/dhat-heap.json".to_owned(), "heap-raw");
        for mode in [
            "position-ascii",
            "position-unicode",
            "snapshot-small",
            "snapshot-medium",
            "snapshot-large",
            "snapshot-medium-unicode",
        ] {
            add(
                format!("memory/line-index-{mode}/dhat-heap.json"),
                "heap-raw",
            );
        }
        for lines in [100, 1000, 10000] {
            for mode in ["snapshot", "fold"] {
                add(
                    format!("memory/folding-{lines}-{mode}/dhat-heap.json"),
                    "heap-raw",
                );
            }
        }
    }
    let scenario = request.scenario.as_str();
    let mode = request.mode.as_str();
    let scenarios = if request.mode == "full" {
        SCENARIOS.as_slice()
    } else {
        std::slice::from_ref(&scenario)
    };
    let full = matches!(mode, "full" | "verification");
    let backends = if full {
        &["cpu", "heap", "native"][..]
    } else {
        std::slice::from_ref(&mode)
    };
    for scenario in scenarios {
        for backend in backends {
            if !matches!(*backend, "cpu" | "heap" | "native") {
                continue;
            }
            add(
                format!("profiles/{scenario}/{backend}/profile-manifest.json"),
                "profile",
            );
            if *backend == "native" && full && !target.contains("linux") {
                add(
                    format!("profiles/{scenario}/native-heap/profile-manifest.json"),
                    "profile",
                );
            }
        }
    }
    result
}

fn files(root: &Path, result: &mut Vec<PathBuf>) -> ToolResult<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files(&entry.path(), result)?;
        } else if kind.is_file() {
            result.push(entry.path());
        } else if kind.is_symlink() {
            return Err(format!(
                "Artifact links must be materialized: {}",
                entry.path().display()
            )
            .into());
        }
    }
    Ok(())
}

fn relative(root: &Path, path: &Path) -> ToolResult<String> {
    Ok(path
        .strip_prefix(root)?
        .to_str()
        .ok_or("Non-Unicode artifact path")?
        .replace('\\', "/"))
}

fn finalize(root: &Path, request: &CollectionRequest, value: &mut Value) -> ToolResult<()> {
    let steps: Value =
        serde_json::from_str(&env::var("STEPS_JSON").unwrap_or_else(|_| "{}".to_owned()))?;
    let mut statuses = Map::new();
    let Value::Object(steps) = steps else {
        return Err("Workflow step metadata must be an object".into());
    };
    for (name, step) in steps {
        let Value::Object(mut step) = step else {
            return Err("Workflow step must be an object".into());
        };
        let outcome = step
            .remove("outcome")
            .ok_or("Missing workflow step outcome")?;
        statuses.insert(
            name,
            Value::Object(Map::from_iter([("status".to_owned(), outcome)])),
        );
    }
    value["statuses"] = Value::Object(statuses);
    let status_file = root.join("collection-status.jsonl");
    if status_file.exists() {
        for line in fs::read_to_string(status_file)?.lines() {
            let mut item: Value = serde_json::from_str(line)?;
            let name = item
                .as_object_mut()
                .ok_or("Malformed collection status")?
                .remove("name")
                .ok_or("Missing collection status name")?;
            let Value::String(name) = name else {
                return Err("Collection status name must be a string".into());
            };
            value["statuses"][name] = item;
        }
    }
    let mut paths = Vec::new();
    files(root, &mut paths)?;
    paths.sort();
    if request.mode == "callgrind" {
        let mut count = 0;
        for path in &paths {
            if path.file_name().is_some_and(|name| name == "summary.json") {
                value["expected"].as_array_mut().ok_or("Expected artifacts must be an array")?.push(
                    json!({"path": relative(root, path)?, "kind": "callgrind", "required": true}));
                count += 1;
            }
        }
        value["statuses"]["callgrind-data"] = json!({"status": if count > 0 {"success"} else {"failure"},
            "error": if count > 0 {Value::Null} else {json!("No raw Callgrind summaries were collected")}});
    }
    for item in value["expected"]
        .as_array_mut()
        .ok_or("Expected artifacts must be an array")?
    {
        let path = item["path"]
            .as_str()
            .ok_or("Missing expected artifact path")?;
        item["present"] = json!(root.join(path).is_file());
    }
    let manifest = root.join("artifact-manifest.json");
    for path in paths {
        if path == manifest {
            continue;
        }
        let item = json!({"path": relative(root, &path)?, "sha256": common::sha256_file(&path)?, "size": fs::metadata(&path)?.len()});
        value["files"]
            .as_array_mut()
            .ok_or("Artifact inventory must be an array")?
            .push(item);
    }
    Ok(())
}

fn manifest(finish: bool) -> ToolResult<()> {
    let request = CollectionRequest::read()?;
    let root = PathBuf::from(variable("PERF_ROOT")?);
    fs::create_dir_all(&root)?;
    let target = variable("PERF_TARGET")?;
    let expected_targets: Vec<&str> = request.targets().map(|target| target.triple).collect();
    let mut value = json!({
        "schema_version": 1, "request_id": request.request_id, "mode": request.mode,
        "base_sha": request.base_sha, "candidate_sha": request.candidate_sha,
        "workflow_sha": variable("WORKFLOW_SHA")?, "repository": variable("GITHUB_REPOSITORY")?,
        "target": target, "runner": variable("PERF_RUNNER")?,
        "run_id": variable("GITHUB_RUN_ID")?, "run_attempt": variable("GITHUB_RUN_ATTEMPT")?,
        "scenario": request.scenario, "native_kind": request.native_kind, "expected_targets": expected_targets,
        "expected": expected(&request, &target), "statuses": {"collection": "pending"}, "files": [],
        "native_heap": if target.contains("linux") {"not-applicable: Linux uses DHAT, not a native heap tracing backend"}
            else {"requested when native profiling is selected"},
    });
    let outcome = if finish {
        finalize(&root, &request, &mut value)
    } else {
        Ok(())
    };
    if let Err(error) = &outcome {
        value["statuses"]["inventory"] = json!({"status": "failure", "error": error.to_string()});
    }
    common::write_json(&root.join("artifact-manifest.json"), &value)?;
    outcome
}

fn status(arguments: &[String]) -> ToolResult<()> {
    let [name, code] = arguments else {
        return Err("workflow status requires collector name and exit code".into());
    };
    let code: i32 = code.parse()?;
    let value = json!({"name": name, "status": if code == 0 {"success"} else {"failure"},
        "exit_code": code, "error": if code == 0 {Value::Null} else {json!(format!("Collection command failed; see logs/{name}.log"))}});
    let root = PathBuf::from(variable("PERF_ROOT")?);
    fs::create_dir_all(&root)?;
    let mut output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("collection-status.jsonl"))?;
    serde_json::to_writer(&mut output, &value)?;
    output.write_all(b"\n")?;
    Ok(())
}

pub(crate) fn run(arguments: &[String]) -> ToolResult<()> {
    common::require_ci()?;
    let (command, rest) = arguments
        .split_first()
        .ok_or("Expected workflow validate, prepare, finalize or status")?;
    match command.as_str() {
        "validate" if rest.is_empty() => validate(),
        "prepare" if rest.is_empty() => manifest(false),
        "finalize" if rest.is_empty() => manifest(true),
        "status" => status(rest),
        _ => Err(format!("Unknown workflow command or unexpected arguments: {command}").into()),
    }
}
