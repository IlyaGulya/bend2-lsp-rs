use super::{Identity, METRICS, PROBE, require, sha256_bytes};
use crate::{ToolResult, common::sha256_file};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

// Parse containers ourselves so duplicate keys remain visible. Primitive syntax,
// string escaping and number representation stay authoritative serde_json work.
// This also preserves literal objects containing serde_json's private number key.
struct StrictParser<'a> {
    text: &'a str,
    offset: usize,
}

impl StrictParser<'_> {
    fn whitespace(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.offset)
            .is_some_and(|byte| b" \t\r\n".contains(byte))
        {
            self.offset += 1;
        }
    }

    fn take(&mut self, byte: u8) -> bool {
        self.whitespace();
        if self.text.as_bytes().get(self.offset) == Some(&byte) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> ToolResult<String> {
        self.whitespace();
        let start = self.offset;
        require(self.take(b'"'), "expected JSON string")?;
        while let Some(byte) = self.text.as_bytes().get(self.offset).copied() {
            self.offset += 1;
            if byte == b'\\' {
                require(self.offset < self.text.len(), "unterminated JSON escape")?;
                self.offset += 1;
            } else if byte == b'"' {
                return Ok(serde_json::from_str(&self.text[start..self.offset])?);
            }
        }
        Err("unterminated JSON string".into())
    }

    fn value(&mut self, depth: usize) -> ToolResult<Value> {
        require(depth <= 128, "JSON nesting limit exceeded")?;
        self.whitespace();
        match self.text.as_bytes().get(self.offset) {
            Some(b'{') => self.object(depth),
            Some(b'[') => {
                self.offset += 1;
                let mut values = Vec::new();
                if !self.take(b']') {
                    loop {
                        values.push(self.value(depth + 1)?);
                        if self.take(b']') {
                            break;
                        }
                        require(self.take(b','), "expected JSON array comma")?;
                    }
                }
                Ok(Value::Array(values))
            }
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(_) => {
                let start = self.offset;
                while self
                    .text
                    .as_bytes()
                    .get(self.offset)
                    .is_some_and(|byte| !byte.is_ascii_whitespace() && !b",]}".contains(byte))
                {
                    self.offset += 1;
                }
                let value: Value = serde_json::from_str(&self.text[start..self.offset])?;
                require(
                    !value.is_number() || value.as_f64().is_some_and(f64::is_finite),
                    "nonfinite JSON number",
                )?;
                Ok(value)
            }
            None => Err("missing JSON value".into()),
        }
    }

    fn object(&mut self, depth: usize) -> ToolResult<Value> {
        self.offset += 1;
        let mut values = Map::new();
        if !self.take(b'}') {
            loop {
                let key = self.string()?;
                require(
                    !values.contains_key(&key),
                    &format!("duplicate JSON object key: {key}"),
                )?;
                require(self.take(b':'), "expected JSON object colon")?;
                values.insert(key, self.value(depth + 1)?);
                if self.take(b'}') {
                    break;
                }
                require(self.take(b','), "expected JSON object comma")?;
            }
        }
        Ok(Value::Object(values))
    }
}

pub(super) fn strict_json(text: &str) -> ToolResult<Value> {
    let mut parser = StrictParser { text, offset: 0 };
    let value = parser.value(0)?;
    parser.whitespace();
    require(parser.offset == text.len(), "trailing JSON data")?;
    Ok(value)
}

pub(super) fn identity(value: &Value) -> ToolResult<Identity> {
    let name = value["function_name"]
        .as_str()
        .filter(|name| !name.trim().is_empty())
        .ok_or("workload function_name must be nonempty")?;
    let identifier = match value.get("id") {
        Some(Value::Null) => None,
        Some(Value::String(identifier)) if !identifier.trim().is_empty() => {
            Some(identifier.clone())
        }
        _ => return Err("workload id must be null or a nonempty string".into()),
    };
    Ok((name.to_owned(), identifier))
}

pub(super) fn parse_measurement(
    text: &str,
    baseline: &str,
    executable: &Path,
    expected: &BTreeSet<Identity>,
) -> ToolResult<(Vec<Value>, Vec<Value>)> {
    let mut workloads = Vec::new();
    let mut summaries = Vec::new();
    let mut seen = BTreeSet::new();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let summary = strict_json(line)?;
        require(
            summary.is_object(),
            "executed summary must be a JSON object",
        )?;
        let id = identity(&summary)?;
        require(
            expected.contains(&id) && seen.insert(id.clone()),
            "duplicate or unexpected executed workload",
        )?;
        require(
            summary["baselines"] == json!([baseline, baseline]),
            "summary does not belong to requested fresh baseline",
        )?;
        require(
            summary["kind"] == "LibraryBenchmark"
                && summary["benchmark_exe"].as_str() == executable.to_str(),
            "summary does not belong to executed library benchmark",
        )?;
        let profiles = summary["profiles"]
            .as_array()
            .ok_or("requires exactly one Callgrind profile")?;
        require(
            profiles.len() == 1 && profiles[0].is_object() && profiles[0]["tool"] == "Callgrind",
            "requires exactly one Callgrind profile",
        )?;
        let parts = profiles[0]["summaries"]["parts"]
            .as_array()
            .ok_or("requires exactly one fresh Callgrind profile part")?;
        require(
            parts.len() == 1,
            "requires exactly one fresh Callgrind profile part",
        )?;
        let metrics = parts[0]["metrics_summary"]["Callgrind"]
            .as_object()
            .ok_or("missing Callgrind metrics")?;
        let mut counts = Map::new();
        for metric in METRICS {
            let values = metrics
                .get(metric)
                .and_then(|value| value["metrics"].as_object())
                .ok_or("metric must be fresh Left-only metrics")?;
            require(
                values.len() == 1 && values.contains_key("Left"),
                "metric must be fresh Left-only metrics, not a stale comparison",
            )?;
            let count = values["Left"]
                .as_object()
                .ok_or("metric count must be a nonnegative integer (not bool)")?;
            require(
                count.len() == 1 && count.contains_key("Int") && count["Int"].as_u64().is_some(),
                "metric count must be a nonnegative integer (not bool)",
            )?;
            counts.insert(metric.to_owned(), count["Int"].clone());
        }
        workloads.push(json!({"function_name": id.0, "id": id.1, "counts": counts}));
        summaries.push(summary);
    }
    require(&seen == expected, "executed measurement missing workloads")?;
    Ok((workloads, summaries))
}

pub(super) fn parse_executable(text: &str, checkout: &Path, target: &Path) -> ToolResult<PathBuf> {
    let mut artifacts = Vec::new();
    let mut finished = false;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let message = strict_json(line)?;
        require(
            message.is_object(),
            "Cargo emitted a non-object JSON message",
        )?;
        if message["reason"] == "build-finished" {
            require(
                message["success"] == true && !finished,
                "Cargo build did not finish successfully exactly once",
            )?;
            finished = true;
        }
        if message["reason"] == "compiler-artifact"
            && message["target"]["name"] == "analysis"
            && message["target"]["kind"] == json!(["bench"])
        {
            let executable = message["executable"]
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or("analysis compiler-artifact has no executable")?;
            let path = Path::new(executable);
            artifacts.push(resolve_path(&if path.is_absolute() {
                path.to_owned()
            } else {
                checkout.join(path)
            })?);
        }
    }
    require(
        finished && artifacts.len() == 1,
        "expected one successful analysis compiler-artifact",
    )?;
    let executable = artifacts.pop().ok_or("missing analysis artifact")?;
    require(
        executable.starts_with(resolve_path(target)?)
            && executable.is_file()
            && is_executable(&fs::metadata(&executable)?),
        "Cargo executable is missing, not executable, or outside its independent target",
    )?;
    Ok(executable)
}

pub(super) fn parse_probe_symbols(text: &str, large: bool) -> ToolResult<Value> {
    let mut symbols = Vec::new();
    for line in text.lines().filter(|line| line.contains(PROBE)) {
        let fields: Vec<_> = line.split_whitespace().collect();
        require(
            fields.len() == 4
                && matches!(fields[2], "t" | "T")
                && fields[3].rsplit("::").next() == Some(PROBE),
            "unsupported nm layout-probe symbol record",
        )?;
        require(
            fields[0].chars().all(|char| char.is_ascii_hexdigit())
                && fields[1].chars().all(|char| char.is_ascii_hexdigit()),
            "unsupported nm layout-probe symbol record",
        )?;
        let address = u64::from_str_radix(fields[0], 16)?;
        let size = u64::from_str_radix(fields[1], 16)?;
        require(address > 0, "layout probe has a zero address")?;
        require(
            size >= if large { 1024 } else { 1 },
            "layout probe has insufficient retained size",
        )?;
        symbols.push(
            json!({"symbol": fields[3], "address": fields[0].to_ascii_lowercase(), "size": size}),
        );
    }
    require(
        symbols.len() == 1,
        "expected one retained layout-probe symbol in nm -C -S output",
    )?;
    symbols.pop().ok_or_else(|| "missing layout probe".into())
}

pub(super) fn parse_workload_list(text: &str) -> ToolResult<BTreeSet<Identity>> {
    let mut identities = BTreeSet::new();
    let mut count = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        if let Some(identity) = line
            .strip_prefix("analysis::analysis_hot_paths::")
            .and_then(|line| line.strip_suffix(": benchmark"))
        {
            let fields: Vec<_> = identity.split("::").collect();
            require(
                (1..=2).contains(&fields.len())
                    && fields.iter().all(|field| {
                        !field.is_empty()
                            && field
                                .chars()
                                .all(|char| char.is_ascii_alphanumeric() || char == '_')
                    }),
                "unsupported benchmark --list output",
            )?;
            let id = (
                fields[0].to_owned(),
                fields.get(1).map(|id| (*id).to_owned()),
            );
            require(
                identities.insert(id),
                "duplicate executed workload in --list",
            )?;
        } else if let Some(number) = line
            .strip_prefix("0 tests, ")
            .and_then(|line| line.strip_suffix(" benchmarks"))
        {
            require(
                count.is_none()
                    && !number.is_empty()
                    && number.chars().all(|char| char.is_ascii_digit()),
                "unsupported benchmark --list output",
            )?;
            count = Some(number.parse::<usize>()?);
        } else {
            return Err(format!("unsupported benchmark --list output: {line}").into());
        }
    }
    require(
        !identities.is_empty() && count == Some(identities.len()),
        "benchmark --list workload count is missing or inconsistent",
    )?;
    require(
        signals().is_subset(&identities),
        "executed harness is missing the three inlay_hints_warm positive-control workloads",
    )?;
    Ok(identities)
}

pub(super) fn signals() -> BTreeSet<Identity> {
    ["small", "medium", "large"]
        .into_iter()
        .map(|size| ("inlay_hints_warm".to_owned(), Some(size.to_owned())))
        .collect()
}

pub(super) fn check_probe_absent(text: &str) -> ToolResult<()> {
    require(
        text.starts_with("# callgrind format")
            && text.lines().any(|line| {
                line.strip_prefix("events:")
                    .is_some_and(|events| events.split_whitespace().any(|event| event == "Ir"))
            }),
        "missing or unsupported raw Callgrind profile",
    )?;
    require(
        !text.contains(PROBE),
        "layout probe appears in collected Callgrind execution; the layout control is invalid",
    )
}

pub(super) fn resolve_path(path: &Path) -> ToolResult<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                resolved.pop();
            }
            Component::CurDir => {}
            other => {
                resolved.push(other.as_os_str());
                if resolved.exists() {
                    resolved = resolved.canonicalize()?;
                }
            }
        }
    }
    Ok(resolved)
}

pub(super) fn validate_directories(source: &Path, work: &Path, output: &Path) -> ToolResult<()> {
    require(source.is_dir(), "source directory does not exist")?;
    for destination in [work, output] {
        require(
            !source.starts_with(destination),
            "work/output directory must not contain the source",
        )?;
        if destination.exists() {
            require(
                destination.is_dir() && fs::read_dir(destination)?.next().is_none(),
                "use a new or empty work/output directory to prevent stale artifact reuse",
            )?;
        }
    }
    require(
        !work.starts_with(output) && !output.starts_with(work),
        "work and output directories must not contain each other",
    )
}

#[cfg(unix)]
fn is_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_metadata: &fs::Metadata) -> bool {
    true
}

fn manifest_walk(
    root: &Path,
    directory: &Path,
    excluded: &[PathBuf],
    files: &mut Vec<Value>,
) -> ToolResult<()> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        let metadata = fs::symlink_metadata(&path)?;
        if (directory == root && matches!(name.to_str(), Some(".git" | ".beads")))
            || (metadata.is_dir()
                && (name == "__pycache__" || directory == root && name == "target"))
        {
            continue;
        }
        if !excluded.is_empty()
            && excluded.iter().any(|exclusion| {
                resolve_path(&path).is_ok_and(|resolved| resolved.starts_with(exclusion))
            })
        {
            continue;
        }
        require(
            !metadata.is_symlink(),
            "source symlinks are not supported in immutable calibration copies",
        )?;
        if metadata.is_dir() {
            manifest_walk(root, &path, excluded, files)?;
        } else {
            require(metadata.is_file(), "source contains a non-regular file")?;
            let relative = path
                .strip_prefix(root)?
                .to_str()
                .ok_or("source path is not UTF-8")?
                .replace('\\', "/");
            files.push(json!({"path": relative, "sha256": sha256_file(&path)?, "executable": is_executable(&metadata)}));
        }
    }
    Ok(())
}

pub(super) fn source_manifest(root: &Path, excluded: &[PathBuf]) -> ToolResult<Value> {
    let mut files = Vec::new();
    manifest_walk(root, root, excluded, &mut files)?;
    files.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    let digest = sha256_bytes(&serde_json::to_vec(&files)?);
    Ok(json!({"sha256": digest, "files": files}))
}
