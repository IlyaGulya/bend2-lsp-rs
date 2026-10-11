use super::{ToolResult, sessions};
use crate::{common, doctor, scenario::Scenario};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path, PathBuf},
    process::{Child, Command},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

fn exact_hex(text: &str, length: usize) -> bool {
    text.len() == length
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn confined(root: &Path, relative: &str) -> ToolResult<PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || relative.contains('\\')
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("CPU profile artifact paths must be portable confined relative paths".into());
    }
    let mut resolved = root.to_owned();
    for component in path.components() {
        resolved.push(component.as_os_str());
        if fs::symlink_metadata(&resolved)?.file_type().is_symlink() {
            return Err("CPU profile artifacts must not contain symlinks".into());
        }
    }
    let canonical = resolved.canonicalize()?;
    if !canonical.starts_with(root) || !canonical.is_file() {
        return Err("CPU profile artifact escapes run directory or is not a file".into());
    }
    Ok(canonical)
}

fn validate_artifacts<'a>(
    root: &Path,
    manifest: &'a Value,
) -> ToolResult<BTreeMap<&'a str, (&'a str, PathBuf)>> {
    let mut artifacts = BTreeMap::new();
    for item in manifest["artifacts"]
        .as_array()
        .ok_or("CPU manifest has no artifact list")?
    {
        let relative = item["path"].as_str().ok_or("Artifact has no path")?;
        let expected = item["sha256"].as_str().ok_or("Artifact has no SHA256")?;
        if !exact_hex(expected, 64) {
            return Err("Artifact SHA256 is not exact lowercase hex".into());
        }
        let path = confined(root, relative)?;
        if common::sha256_file(&path)? != expected {
            return Err(format!("CPU artifact SHA256 mismatch: {relative}").into());
        }
        if artifacts.insert(relative, (expected, path)).is_some() {
            return Err("CPU manifest contains duplicate artifact paths".into());
        }
    }
    if artifacts.is_empty() {
        return Err("CPU manifest has no preserved artifacts".into());
    }
    Ok(artifacts)
}

fn preserved_identity<'a>(
    item: &Value,
    artifacts: &'a BTreeMap<&str, (&str, PathBuf)>,
) -> ToolResult<&'a Path> {
    let relative = item["artifact_path"]
        .as_str()
        .ok_or("Binary/symbol identity has no packaged artifact path")?;
    let hash = item["sha256"]
        .as_str()
        .ok_or("Binary/symbol identity has no SHA256")?;
    let (artifact_hash, path) = artifacts
        .get(relative)
        .ok_or("Binary/symbol artifact is absent from manifest")?;
    if hash != *artifact_hash {
        return Err("Packaged binary/symbol differs from its recorded original identity".into());
    }
    Ok(path)
}

fn symbol_directories(
    manifest: &Value,
    artifacts: &BTreeMap<&str, (&str, PathBuf)>,
) -> ToolResult<Vec<PathBuf>> {
    let binary = preserved_identity(&manifest["binary"], artifacts)?;
    let mut directories = vec![
        binary
            .parent()
            .ok_or("Packaged executable has no directory")?
            .to_owned(),
    ];
    for symbol in manifest["binary"]["symbols"]
        .as_array()
        .ok_or("Binary identity has no symbol list")?
    {
        let path = preserved_identity(symbol, artifacts)?;
        let directory = path.parent().ok_or("Packaged symbols have no directory")?;
        if !directories.iter().any(|existing| existing == directory) {
            directories.push(directory.to_owned());
        }
    }
    Ok(directories)
}

fn validate_manifest(manifest: &Value) -> ToolResult<u32> {
    if manifest["format_version"] != 1
        || manifest["status"] != "complete"
        || manifest["backend"] != "samply"
    {
        return Err(
            "CPU viewing requires a complete supported samply profile-manifest.json".into(),
        );
    }
    if manifest["profile_kind"] != "cpu"
        || manifest["errors"]
            .as_array()
            .is_none_or(|errors| !errors.is_empty())
    {
        return Err("CPU manifest is incomplete or contains collection errors".into());
    }
    if !exact_hex(
        manifest["source_sha"]
            .as_str()
            .ok_or("CPU manifest source SHA absent")?,
        40,
    ) {
        return Err("CPU manifest source SHA is not an exact revision".into());
    }
    let target = manifest["target"]
        .as_str()
        .ok_or("CPU manifest target absent")?;
    if !matches!(
        target,
        "x86_64-unknown-linux-gnu"
            | "aarch64-unknown-linux-gnu"
            | "x86_64-apple-darwin"
            | "aarch64-apple-darwin"
            | "x86_64-pc-windows-msvc"
            | "aarch64-pc-windows-msvc"
    ) {
        return Err("CPU manifest native target is unsupported".into());
    }
    Scenario::parse(
        manifest["scenario"]
            .as_str()
            .ok_or("CPU manifest scenario absent")?,
    )?;
    let pid = manifest["target_pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0)
        .ok_or("CPU manifest has no valid actual target PID")?;
    let recorded_version = manifest["profiler"]["version"]
        .as_str()
        .ok_or("CPU manifest has no profiler version")?;
    if !recorded_version
        .split_whitespace()
        .any(|word| word == doctor::SAMPLY_VERSION)
    {
        return Err("CPU manifest was not recorded with the supported pinned sampler".into());
    }
    Ok(pid)
}

pub(super) fn open(path: &Path) -> ToolResult<()> {
    let path = path.canonicalize()?;
    let root = path.parent().ok_or("CPU manifest has no run directory")?;
    let manifest: Value = serde_json::from_reader(fs::File::open(&path)?)?;
    let pid = validate_manifest(&manifest)?;
    let artifacts = validate_artifacts(root, &manifest)?;
    let directories = symbol_directories(&manifest, &artifacts)?;
    let (_, profile) = artifacts
        .get("samply.json")
        .ok_or("CPU manifest does not preserve samply.json")?;
    let samples: Value = serde_json::from_reader(fs::File::open(profile)?)?;
    if !sessions::has_samples(&samples, pid) {
        return Err("Preserved CPU profile has no actual recorded target-PID samples".into());
    }
    let version = doctor::output("samply", &["--version"]).map_err(|error| format!("Pinned samply {} is required to view CPU traces: {error}. Install explicitly: cargo install --locked --git https://github.com/mstange/samply --rev da75c28f367454c621e690eeb4e44ec2ebb29a78 samply", doctor::SAMPLY_VERSION))?;
    if !version
        .split_whitespace()
        .any(|word| word == doctor::SAMPLY_VERSION)
    {
        return Err(format!(
            "Viewer requires samply {}; found {version}",
            doctor::SAMPLY_VERSION
        )
        .into());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancelled);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    let mut command = Command::new("samply");
    command
        .arg("load")
        .arg(profile)
        .args(["--address", "127.0.0.1"]);
    for directory in directories {
        command.arg("--symbol-dir").arg(directory);
    }
    let mut viewer = Viewer(command.spawn()?);
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(status) = viewer.0.try_wait()? {
            if !status.success() {
                return Err(format!("Samply viewer exited unsuccessfully: {status}").into());
            }
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
}

struct Viewer(Child);
impl Drop for Viewer {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn downloaded_profile_hash_mismatch_prevents_viewing() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let profile = directory.path().join("samply.json");
        fs::write(&profile, b"original profile bytes")?;
        let hash = common::sha256_file(&profile)?;
        let manifest = json!({"artifacts":[{"path":"samply.json","sha256":hash}]});
        let root = directory.path().canonicalize()?;
        validate_artifacts(&root, &manifest)?;
        fs::write(&profile, b"changed profile bytes")?;
        assert!(validate_artifacts(&root, &manifest).is_err());
        Ok(())
    }

    #[test]
    fn downloaded_paths_cannot_escape_run_or_use_windows_separators() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().canonicalize()?;
        for path in [
            "../samply.json",
            "/tmp/samply.json",
            "symbols\\bend2-lsp.pdb",
            "",
        ] {
            assert!(confined(&root, path).is_err());
        }
        Ok(())
    }

    #[test]
    fn preserved_symbol_identity_must_match_original() -> ToolResult<()> {
        let directory = tempfile::tempdir()?;
        let binary = directory.path().join("bend2-lsp");
        fs::write(&binary, b"symbolized executable")?;
        let hash = common::sha256_file(&binary)?;
        let manifest = json!({"artifacts":[{"path":"bend2-lsp","sha256":hash}]});
        let root = directory.path().canonicalize()?;
        let artifacts = validate_artifacts(&root, &manifest)?;
        let identity = json!({"artifact_path":"bend2-lsp","sha256":hash});
        assert_eq!(
            preserved_identity(&identity, &artifacts)?,
            binary.canonicalize()?
        );
        let wrong = json!({"artifact_path":"bend2-lsp","sha256":"0".repeat(64)});
        assert!(preserved_identity(&wrong, &artifacts).is_err());
        Ok(())
    }
}
