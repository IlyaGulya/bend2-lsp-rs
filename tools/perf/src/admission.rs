//! Validate retained measurements and the exact human-review environment, without collecting.
use crate::{ToolResult, common, paired, policy, repeated::METHOD};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
};

#[derive(Parser)]
#[command(
    no_binary_name = true,
    about = "Validate performance admission evidence locally; never measure or approve a run"
)]
struct Args {
    #[command(subcommand)]
    command: AdmissionCommand,
}

#[derive(Subcommand)]
enum AdmissionCommand {
    /// Classify retained evidence; successful validation is not performance approval.
    Measurement(MeasurementArgs),
    /// Require the repository owner's sole User review on performance-exception.
    Environment {
        #[arg(long)]
        owner_id: u64,
        #[arg(long)]
        input: PathBuf,
    },
}

#[derive(clap::Args)]
struct MeasurementArgs {
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    base_sha: String,
    #[arg(long)]
    candidate_sha: String,
    #[arg(long)]
    head_sha: String,
    #[arg(long)]
    run_id: String,
    #[arg(long)]
    benchmark_exit_code: u8,
    #[arg(long)]
    policy_exit_code: u8,
    /// Require the active method instead of accepting a historical single-pair bundle.
    #[arg(long, value_parser = [METHOD])]
    measurement_method: Option<String>,
}

pub(crate) fn run(args: &[String]) -> ToolResult<()> {
    match Args::try_parse_from(args)?.command {
        AdmissionCommand::Measurement(args) => {
            println!("{}", measurement(&args)?);
            Ok(())
        }
        AdmissionCommand::Environment { owner_id, input } => {
            validate_environment(owner_id, &read_json(&input)?)
        }
    }
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

fn positive_id(value: &str) -> bool {
    !value.starts_with('0')
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|id| id > 0)
}

// Match the dashboard's relative-path and canonical confinement conventions.
fn confined(root: &Path, name: &str) -> ToolResult<PathBuf> {
    if name.is_empty()
        || name.contains('\\')
        || Path::new(name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!("Artifact path is not a confined relative path: {name}").into());
    }
    let path = root.join(name).canonicalize()?;
    if !path.starts_with(root) || !path.is_file() {
        return Err(format!("Artifact escapes measurement bundle or is not a file: {name}").into());
    }
    Ok(path)
}

fn verify_inventory<'a>(
    root: &Path,
    manifest: &'a Value,
) -> ToolResult<BTreeMap<&'a str, &'a Value>> {
    let mut inventory = BTreeMap::new();
    for item in manifest["files"]
        .as_array()
        .ok_or("Missing artifact inventory")?
    {
        let name = item["path"].as_str().ok_or("Missing artifact path")?;
        if inventory.insert(name, item).is_some() {
            return Err("Duplicate file identity in manifest".into());
        }
        let path = confined(root, name)?;
        let hash = item["sha256"].as_str().ok_or("Missing artifact SHA256")?;
        if !hex(hash, 64) || common::sha256_file(&path)? != hash {
            return Err(format!("Artifact SHA256 mismatch: {name}").into());
        }
        if item["size"].as_u64() != Some(fs::metadata(path)?.len()) {
            return Err(format!("Artifact size mismatch: {name}").into());
        }
    }
    let expected = manifest["expected"]
        .as_array()
        .ok_or("Missing expected artifacts")?;
    let mut seen = std::collections::BTreeSet::new();
    for item in expected {
        let name = item["path"]
            .as_str()
            .ok_or("Missing expected artifact path")?;
        if !seen.insert(name) {
            return Err("Duplicate expected artifact path".into());
        }
        let required = item["required"]
            .as_bool()
            .ok_or("Missing artifact requirement")?;
        if required && (item["present"] != true || !inventory.contains_key(name)) {
            return Err(format!("Missing required artifact: {name}").into());
        }
    }
    if expected.is_empty() {
        return Err("Empty expected artifact inventory".into());
    }
    Ok(inventory)
}

fn validate_source(args: &MeasurementArgs, manifest: &Value) -> ToolResult<()> {
    if manifest["schema_version"] != 1 || manifest["mode"] != "callgrind" {
        return Err("Admission requires a canonical Callgrind artifact manifest".into());
    }
    for (field, expected) in [
        ("base_sha", args.base_sha.as_str()),
        ("candidate_sha", args.candidate_sha.as_str()),
    ] {
        if !hex(expected, 40) || manifest[field].as_str() != Some(expected) {
            return Err(format!("Admission {field} differs from exact source provenance").into());
        }
    }
    let gate = &manifest["gate_identity"];
    if !hex(&args.head_sha, 40)
        || gate["head_sha"].as_str() != Some(args.head_sha.as_str())
        || gate["workflow"] != "performance"
        || gate["job"] != "compare"
        || gate["step"] != "Compare pull request with main"
    {
        return Err(
            "Admission head SHA or canonical gate identity differs from requested run".into(),
        );
    }
    if !positive_id(&args.run_id) || manifest["run_id"].as_str() != Some(args.run_id.as_str()) {
        return Err("Admission run ID differs from exact run provenance".into());
    }
    if !manifest["workflow_sha"]
        .as_str()
        .is_some_and(|sha| hex(sha, 40))
        || !manifest["run_attempt"].as_str().is_some_and(positive_id)
    {
        return Err("Missing or malformed workflow SHA or run attempt".into());
    }
    Ok(())
}

fn validate_outcomes(args: &MeasurementArgs, manifest: &Value, classification: &str) -> ToolResult<()> {
    let repeated = manifest["measurement_method"] == METHOD;
    let policy_exit = match classification {
        "passed" => 0,
        "regression" => 1,
        "inconclusive" if repeated => 3,
        _ => return Err("Invalid measurement classification".into()),
    };
    let benchmark_valid = if repeated {
        args.benchmark_exit_code == 0
    } else {
        matches!(args.benchmark_exit_code, 0 | 3)
    };
    if !benchmark_valid
        || args.policy_exit_code != policy_exit
        || (policy_exit == 0 && args.benchmark_exit_code != 0)
    {
        return Err(
            "Command exits disagree with validated metrics or report a collector error".into(),
        );
    }
    let statuses = manifest["statuses"]
        .as_object()
        .ok_or("Missing workflow collection outcomes")?;
    let candidate = statuses
        .get("candidate")
        .ok_or("Missing candidate outcome")?;
    if candidate["benchmark_exit_code"].as_u64() != Some(u64::from(args.benchmark_exit_code))
        || candidate["policy_exit_code"].as_u64() != Some(u64::from(args.policy_exit_code))
        || candidate["status"] != if policy_exit == 0 { "success" } else { "failure" }
    {
        return Err("Candidate outcome or retained command exits are inconsistent".into());
    }
    for required in ["baseline", "callgrind-data"] {
        if statuses
            .get(required)
            .and_then(|value| value["status"].as_str())
            != Some("success")
        {
            return Err(
                format!("Missing or failed required collection outcome: {required}").into(),
            );
        }
    }
    for (name, value) in statuses {
        if name == "candidate" {
            continue;
        }
        let status = value.as_str().or_else(|| value["status"].as_str());
        if !matches!(
            status,
            Some("success" | "complete" | "passed" | "skipped" | "not-applicable")
        ) {
            return Err(format!("Unsuccessful collection outcome: {name}: {value}").into());
        }
    }
    Ok(())
}

fn measurement(args: &MeasurementArgs) -> ToolResult<Value> {
    let root = args.root.canonicalize()?;
    if !root.is_dir() {
        return Err("Measurement root must be a directory".into());
    }
    let manifest = read_json(&confined(&root, "artifact-manifest.json")?)?;
    validate_source(args, &manifest)?;
    if let Some(required) = &args.measurement_method
        && manifest["measurement_method"].as_str() != Some(required.as_str())
    {
        return Err("Artifact does not use the required measurement method".into());
    }
    let inventory = verify_inventory(&root, &manifest)?;
    if !inventory.contains_key("callgrind-policy.json") {
        return Err("Policy verdict is absent from manifest inventory".into());
    }
    let verdict = read_json(&confined(&root, "callgrind-policy.json")?)?;
    let (classification, failures) = if manifest.get("measurement_method").is_some() {
        if manifest["measurement_method"] != METHOD {
            return Err("Unsupported retained measurement method".into());
        }
        let failures = paired::report::validate(&root, &manifest, &verdict)?;
        let classification = match verdict["status"].as_str() {
            Some("complete") => "passed",
            Some("regression") => "regression",
            Some("inconclusive") => "inconclusive",
            _ => return Err("Invalid paired measurement outcome".into()),
        };
        (classification, failures)
    } else {
        let mut identities = vec![&verdict["inputs"]["baseline_manifest"]];
        identities.extend(
            verdict["inputs"]["summaries"]
                .as_array()
                .ok_or("Missing policy summary inventory")?,
        );
        let mut inputs = BTreeMap::new();
        for identity in identities {
            let name = identity["path"]
                .as_str()
                .ok_or("Missing policy input path")?;
            let item = inventory
                .get(name)
                .ok_or("Policy input is absent from manifest inventory")?;
            if item["sha256"] != identity["sha256"] || item["size"] != identity["size"] {
                return Err("Policy input identity differs from manifest inventory".into());
            }
            if inputs
                .insert(name.to_owned(), read_json(&confined(&root, name)?)?)
                .is_some()
            {
                return Err("Duplicate policy input identity".into());
            }
        }
        let failures = policy::validate_verdict(&verdict, &manifest, &inputs)?;
        let classification = if failures.is_empty() { "passed" } else { "regression" };
        (classification, failures.into_iter().map(Value::String).collect())
    };
    validate_outcomes(args, &manifest, classification)?;
    let mut result = json!({
        "status": classification,
        "base_sha": args.base_sha, "candidate_sha": args.candidate_sha,
        "head_sha": args.head_sha, "run_id": args.run_id, "failures": failures
    });
    if let Some(method) = manifest.get("measurement_method") {
        result["method"] = method.clone();
    }
    Ok(result)
}

fn validate_environment(owner_id: u64, environment: &Value) -> ToolResult<()> {
    if owner_id == 0 || environment["name"] != "performance-exception" {
        return Err("Expected performance-exception environment and a positive owner ID".into());
    }
    if environment["can_admins_bypass"] != false {
        return Err("Environment must explicitly disallow administrator review bypass".into());
    }
    let rules = environment["protection_rules"]
        .as_array()
        .ok_or("Missing environment protection rules")?;
    let mut required_reviewers = rules
        .iter()
        .filter(|rule| rule["type"] == "required_reviewers");
    let rule = required_reviewers
        .next()
        .ok_or("Environment has no required reviewers")?;
    if required_reviewers.next().is_some() {
        return Err("Environment has multiple required-reviewer rules".into());
    }
    let reviewers = rule["reviewers"]
        .as_array()
        .ok_or("Missing required reviewer inventory")?;
    if reviewers.len() != 1
        || reviewers[0]["type"] != "User"
        || reviewers[0]["reviewer"]["id"].as_u64() != Some(owner_id)
    {
        return Err("Environment must require exactly the repository owner's User review".into());
    }
    Ok(())
}
