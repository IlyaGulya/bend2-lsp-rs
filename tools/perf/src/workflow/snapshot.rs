//! Publish collection evidence only after the owned process group is absent.
use super::collection;
use crate::{ToolResult, callgrind, common};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

const MANIFEST: &str = "snapshot-publication.json";
const RECEIPT: &str = "collection-lifecycle.json";
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;
const MAX_ERROR_CHARS: usize = 4096;
const UNSUPPORTED: &str =
    "Atomic no-clobber snapshot publication requires Linux GNU; no evidence was published";

#[derive(Parser)]
#[command(name = "workflow seal")]
struct Arguments {
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    output: PathBuf,
}

struct Publication {
    source: PathBuf,
    output: PathBuf,
    parent: PathBuf,
}

struct Evidence {
    files: Vec<Value>,
    completed: Vec<String>,
    incomplete: Vec<String>,
}

fn present(path: &Path) -> ToolResult<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn regular(metadata: &Metadata, path: &Path, directory: bool) -> ToolResult<()> {
    callgrind::require(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        &format!(
            "Expected an ordinary {} without links: {}",
            if directory { "directory" } else { "file" },
            path.display()
        ),
    )
}

impl Publication {
    fn prepare(source: &Path, output: &Path) -> ToolResult<Self> {
        callgrind::require(
            present(output)?.is_none(),
            "Snapshot destination already exists",
        )?;
        let resolved_source = callgrind::resolve_path(source)?;
        let output = callgrind::resolve_path(output)?;
        callgrind::require(
            !resolved_source.starts_with(&output) && !output.starts_with(&resolved_source),
            "Snapshot source and destination must not overlap",
        )?;
        let parent = output
            .parent()
            .ok_or("Snapshot destination has no parent")?
            .to_owned();
        Ok(Self {
            source: source.to_owned(),
            output,
            parent,
        })
    }

    fn stage(&self) -> ToolResult<tempfile::TempDir> {
        callgrind::require(
            cfg!(all(target_os = "linux", target_env = "gnu")),
            UNSUPPORTED,
        )?;
        fs::create_dir_all(&self.parent)?;
        Ok(tempfile::Builder::new()
            .prefix(".callgrind-snapshot-")
            .tempdir_in(&self.parent)?)
    }

    fn publish(&self, stage: &Path) -> ToolResult<()> {
        callgrind::require(
            present(&self.output)?.is_none(),
            "Snapshot destination already exists",
        )?;
        rename_new(&self.parent, stage, &self.output)
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn rename_new(parent: &Path, stage: &Path, output: &Path) -> ToolResult<()> {
    let directory = File::open(parent)?;
    nix::fcntl::renameat2(
        &directory,
        stage.file_name().ok_or("Snapshot stage has no filename")?,
        &directory,
        output
            .file_name()
            .ok_or("Snapshot destination has no filename")?,
        nix::fcntl::RenameFlags::RENAME_NOREPLACE,
    )?;
    Ok(())
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn rename_new(_parent: &Path, _stage: &Path, _output: &Path) -> ToolResult<()> {
    Err(UNSUPPORTED.into())
}

fn open_regular(path: &Path) -> ToolResult<File> {
    regular(&fs::symlink_metadata(path)?, path, false)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    regular(&file.metadata()?, path, false)?;
    Ok(file)
}

fn unchanged(before: &Metadata, after: &Metadata, path: &Path) -> ToolResult<()> {
    let same = before.len() == after.len() && before.modified()? == after.modified()?;
    #[cfg(unix)]
    let same = {
        use std::os::unix::fs::MetadataExt as _;
        same && before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    };
    callgrind::require(
        same,
        &format!("Source changed while sealing: {}", path.display()),
    )
}

fn read_receipt(source: &Path) -> ToolResult<Value> {
    regular(&fs::symlink_metadata(source)?, source, true)?;
    let path = source.join(RECEIPT);
    let mut file = open_regular(&path)?;
    let before = file.metadata()?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    callgrind::require(
        u64::try_from(bytes.len())? <= MAX_RECEIPT_BYTES,
        "Collection lifecycle receipt is too large",
    )?;
    unchanged(&before, &file.metadata()?, &path)?;
    unchanged(&before, &fs::symlink_metadata(&path)?, &path)?;
    callgrind::strict_json(std::str::from_utf8(&bytes)?)
}

fn absent_group(receipt: &Value) -> ToolResult<()> {
    callgrind::require(
        receipt["schema_version"] == 1 && receipt["cleanup_complete"] == true,
        "Collection lifecycle does not prove complete cleanup",
    )?;
    callgrind::require(
        receipt["exit_code"].as_i64().is_some() && receipt["timed_out"].is_boolean(),
        "Collection lifecycle has no terminal exit status",
    )?;
    let group = match receipt["spawned"].as_bool() {
        Some(true) => receipt["process_group"]
            .as_u64()
            .and_then(|group| u32::try_from(group).ok())
            .filter(|group| *group > 0)
            .ok_or("Spawned collection has no valid owned process group")?,
        Some(false) if receipt.get("process_group").is_some_and(Value::is_null) => {
            return Err("Collection never spawned; raw evidence cannot be admitted".into());
        }
        _ => {
            return Err(
                "Collection lifecycle has inconsistent spawned/process_group fields".into(),
            );
        }
    };
    callgrind::require(
        collection::group_absent(group)?,
        "Owned collection process group is still present",
    )
}

fn relative(root: &Path, path: &Path) -> ToolResult<String> {
    let relative = path
        .strip_prefix(root)?
        .to_str()
        .ok_or("Non-Unicode evidence path")?
        .to_owned();
    #[cfg(windows)]
    let relative = relative.replace('\\', "/");
    Ok(relative)
}

fn copy_file(source: &Path, destination: &Path) -> ToolResult<(u64, String)> {
    let mut input = open_regular(source)?;
    let before = input.metadata()?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let size = io::copy(&mut input, &mut output)?;
    callgrind::require(size == before.len(), "Evidence size changed while copying")?;
    unchanged(&before, &input.metadata()?, source)?;
    let after = fs::symlink_metadata(source)?;
    regular(&after, source, false)?;
    unchanged(&before, &after, source)?;
    output.sync_all()?;
    drop(output);
    Ok((size, common::sha256_file(destination)?))
}

fn copy_tree(
    source: &Path,
    destination: &Path,
    source_root: &Path,
    stage: &Path,
    category: &str,
    evidence: &mut Evidence,
) -> ToolResult<()> {
    let before = fs::symlink_metadata(source)?;
    if before.is_dir() {
        fs::create_dir(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                source_root,
                stage,
                category,
                evidence,
            )?;
        }
        unchanged(&before, &fs::symlink_metadata(source)?, source)?;
        File::open(destination)?.sync_all()?;
    } else {
        regular(&before, source, false)?;
        let (size, sha256) = copy_file(source, destination)?;
        evidence.files.push(json!({
            "source_path": relative(source_root, source)?, "path": relative(stage, destination)?,
            "category": category, "size": size, "sha256": sha256,
        }));
    }
    Ok(())
}

fn copy_raw(source: &Path, stage: &Path, evidence: &mut Evidence) -> ToolResult<()> {
    let raw = source.join("raw");
    let Some(before) = present(&raw)? else {
        return Ok(());
    };
    regular(&before, &raw, true)?;
    fs::create_dir(stage.join("raw"))?;
    let mut samples = fs::read_dir(&raw)?.collect::<Result<Vec<_>, _>>()?;
    samples.sort_by_key(fs::DirEntry::file_name);
    for entry in samples {
        let sample = entry.path();
        regular(&fs::symlink_metadata(&sample)?, &sample, true)?;
        let marker = sample.join("sample.json");
        let (destination, category) = if let Some(metadata) = present(&marker)? {
            regular(&metadata, &marker, false)?;
            let mut text = String::new();
            open_regular(&marker)?.read_to_string(&mut text)?;
            callgrind::require(
                callgrind::strict_json(&text)?.is_object(),
                "Completed sample marker must be a JSON object",
            )?;
            evidence.completed.push(relative(source, &sample)?);
            (stage.join("raw").join(entry.file_name()), "completed-raw")
        } else {
            let diagnostic = stage.join("incomplete/raw");
            fs::create_dir_all(&diagnostic)?;
            evidence.incomplete.push(relative(source, &sample)?);
            (diagnostic.join(entry.file_name()), "diagnostic-incomplete")
        };
        copy_tree(&sample, &destination, source, stage, category, evidence)?;
    }
    unchanged(&before, &fs::symlink_metadata(&raw)?, &raw)
}

fn fill(publication: &Publication, stage: &Path) -> ToolResult<()> {
    let source = &publication.source;
    // This proof deliberately precedes even enumerating the raw directory.
    let receipt = read_receipt(source)?;
    absent_group(&receipt)?;
    let before = fs::symlink_metadata(source)?;
    let mut evidence = Evidence {
        files: Vec::new(),
        completed: Vec::new(),
        incomplete: Vec::new(),
    };
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        callgrind::require(
            name != MANIFEST && name != "incomplete",
            "Source uses a reserved snapshot publication namespace",
        )?;
        if name != "raw" {
            copy_tree(
                &entry.path(),
                &stage.join(name),
                source,
                stage,
                "metadata",
                &mut evidence,
            )?;
        }
    }
    absent_group(&receipt)?;
    copy_raw(source, stage, &mut evidence)?;
    unchanged(&before, &fs::symlink_metadata(source)?, source)?;
    callgrind::require(
        read_receipt(source)? == receipt,
        "Collection lifecycle receipt changed while sealing",
    )?;
    absent_group(&receipt)?;
    evidence
        .files
        .sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    evidence.completed.sort();
    evidence.incomplete.sort();
    common::write_json(
        &stage.join(MANIFEST),
        &json!({
            "schema_version": 1, "kind": "callgrind-evidence-snapshot", "status": "published",
            "source": callgrind::resolve_path(source)?, "source_untouched": true,
            "collection_lifecycle": receipt, "files": evidence.files,
            "completed_samples": evidence.completed, "incomplete_samples": evidence.incomplete,
            "incomplete_namespace": "incomplete/raw", "incomplete_reason": "sample.json was not committed",
            "metadata_policy": "copied verbatim; original dataset and admission checks remain authoritative",
            "partial_evidence_grants_admission": false,
        }),
    )?;
    sync_stage(stage)
}

fn sync_stage(stage: &Path) -> ToolResult<()> {
    File::open(stage.join(MANIFEST))?.sync_all()?;
    // Nested ordinary directories were synced after copying. These two may be
    // created separately when incomplete samples are quarantined.
    for directory in [
        stage.join("raw"),
        stage.join("incomplete/raw"),
        stage.join("incomplete"),
    ] {
        if present(&directory)?.is_some() {
            File::open(directory)?.sync_all()?;
        }
    }
    File::open(stage)?.sync_all()?;
    Ok(())
}

fn seal(source: &Path, output: &Path) -> ToolResult<()> {
    let publication = Publication::prepare(source, output)?;
    let stage = publication.stage()?;
    if let Err(error) = fill(&publication, stage.path()) {
        // Never turn a partially copied tree into a published artifact. A fresh
        // sibling contains only the bounded, truthful failure envelope.
        let cleanup_error = stage.close().err().map(|error| {
            error
                .to_string()
                .chars()
                .take(MAX_ERROR_CHARS)
                .collect::<String>()
        });
        let failure = publication.stage()?;
        common::write_json(
            &failure.path().join(MANIFEST),
            &json!({
                "schema_version": 1, "kind": "callgrind-evidence-snapshot", "status": "failure",
                "source": source, "source_untouched": true, "raw_published": false,
                "partial_evidence_grants_admission": false,
                "error": error.to_string().chars().take(MAX_ERROR_CHARS).collect::<String>(),
                "stage_cleanup_error": cleanup_error,
            }),
        )?;
        sync_stage(failure.path())?;
        publication.publish(failure.path())?;
        return Err(error);
    }
    publication.publish(stage.path())
}

pub(super) fn run(arguments: &[String]) -> ToolResult<()> {
    let arguments = Arguments::try_parse_from(
        std::iter::once("seal").chain(arguments.iter().map(String::as_str)),
    )?;
    seal(&arguments.source, &arguments.output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn read_json(path: &Path) -> ToolResult<Value> {
        Ok(serde_json::from_reader(File::open(path)?)?)
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn fixture() -> ToolResult<(tempfile::TempDir, PathBuf, PathBuf)> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let output = temporary.path().join("published");
        fs::create_dir(&source)?;
        // Prove the fixture group absent rather than assume an arbitrary PID.
        let group = (1_000_000..1_001_000)
            .find_map(|group| match collection::group_absent(group) {
                Ok(true) => Some(Ok(group)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .ok_or("No absent process group available")??;
        common::write_json(
            &source.join(RECEIPT),
            &json!({
                "schema_version": 1, "spawned": true, "process_group": group,
                "cleanup_complete": true, "exit_code": 0, "timed_out": false,
                "command": ["original-benchmark", "--original-profile-path"],
            }),
        )?;
        Ok((temporary, source, output))
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn sample(source: &Path, name: &str, completed: bool) -> ToolResult<PathBuf> {
        let sample = source.join("raw").join(name);
        let benchmark = sample.join("analysis/iai");
        fs::create_dir_all(&benchmark)?;
        fs::write(benchmark.join("callgrind.out"), b"immutable profile\n")?;
        fs::write(
            sample.join("analysis/stdout.jsonl"),
            b"original executed command\n",
        )?;
        if completed {
            common::write_json(
                &sample.join("sample.json"),
                &json!({"pair": 1, "variant": "a", "benchmarks": {"analysis": {"raw_stdout": format!("raw/{name}/analysis/stdout.jsonl")}}}),
            )?;
        }
        Ok(sample)
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn completed_and_incomplete_evidence_have_separate_namespaces() -> ToolResult<()> {
        let (_temporary, source, output) = fixture()?;
        sample(&source, "pair001a", true)?;
        sample(&source, "pair001b", false)?;
        let metadata = b"{\"samples\":[],\"executed\":\"/original/path\"}\n";
        fs::write(source.join("partial-data.json"), metadata)?;
        fs::write(
            source.join("failure.json"),
            b"{\"error\":\"original failure\"}\n",
        )?;
        seal(&source, &output)?;
        assert!(output.join("raw/pair001a/sample.json").is_file());
        assert!(!output.join("raw/pair001b").exists());
        assert!(
            output
                .join("incomplete/raw/pair001b/analysis/iai/callgrind.out")
                .is_file()
        );
        assert!(!output.join("data.json").exists());
        assert_eq!(fs::read(output.join("partial-data.json"))?, metadata);
        assert_eq!(
            fs::read(source.join("failure.json"))?,
            fs::read(output.join("failure.json"))?
        );
        let manifest = read_json(&output.join(MANIFEST))?;
        assert_eq!(manifest["completed_samples"], json!(["raw/pair001a"]));
        assert_eq!(manifest["incomplete_samples"], json!(["raw/pair001b"]));
        assert_eq!(manifest["partial_evidence_grants_admission"], false);
        assert_eq!(
            manifest["collection_lifecycle"]["command"][1],
            "--original-profile-path"
        );
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn published_files_are_independent_of_later_source_changes() -> ToolResult<()> {
        let (_temporary, source, output) = fixture()?;
        let original = sample(&source, "pair001a", true)?;
        fs::write(source.join("data.json"), b"{\"samples\":[]}\n")?;
        seal(&source, &output)?;
        let profile = output.join("raw/pair001a/analysis/iai/callgrind.out");
        let digest = common::sha256_file(&profile)?;
        fs::write(
            original.join("analysis/iai/callgrind.out"),
            b"later mutation",
        )?;
        fs::remove_file(source.join("data.json"))?;
        assert_eq!(common::sha256_file(&profile)?, digest);
        assert_eq!(fs::read(output.join("data.json"))?, b"{\"samples\":[]}\n");
        let manifest = read_json(&output.join(MANIFEST))?;
        assert!(
            manifest["files"]
                .as_array()
                .ok_or("Missing file inventory")?
                .iter()
                .any(
                    |file| file["path"] == "raw/pair001a/analysis/iai/callgrind.out"
                        && file["sha256"] == digest
                )
        );
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn assert_failure_only(output: &Path) -> ToolResult<Value> {
        let entries = fs::read_dir(output)?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), MANIFEST);
        let manifest = read_json(&output.join(MANIFEST))?;
        assert_eq!(manifest["status"], "failure");
        assert_eq!(manifest["raw_published"], false);
        Ok(manifest)
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn uncertain_cleanup_is_rejected_before_raw_walking() -> ToolResult<()> {
        use std::os::unix::fs::symlink;
        let (_temporary, source, output) = fixture()?;
        let mut receipt = read_json(&source.join(RECEIPT))?;
        receipt["cleanup_complete"] = false.into();
        common::write_json(&source.join(RECEIPT), &receipt)?;
        symlink("missing-and-must-not-be-walked", source.join("raw"))?;
        assert!(seal(&source, &output).is_err());
        let failure = assert_failure_only(&output)?;
        assert!(
            failure["error"]
                .as_str()
                .ok_or("Missing error")?
                .contains("complete cleanup")
        );
        assert!(fs::symlink_metadata(source.join("raw"))?.is_symlink());
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn live_owned_group_blocks_raw_even_with_completed_receipt() -> ToolResult<()> {
        let (_temporary, source, output) = fixture()?;
        sample(&source, "pair001a", true)?;
        let stat = fs::read_to_string("/proc/self/stat")?;
        let fields = stat
            .rsplit_once(')')
            .ok_or("Invalid process stat")?
            .1
            .split_whitespace()
            .collect::<Vec<_>>();
        let group: u32 = fields.get(2).ok_or("Missing process group")?.parse()?;
        let mut receipt = read_json(&source.join(RECEIPT))?;
        receipt["process_group"] = group.into();
        common::write_json(&source.join(RECEIPT), &receipt)?;
        assert!(seal(&source, &output).is_err());
        assert_failure_only(&output)?;
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn never_spawned_receipt_cannot_admit_existing_raw() -> ToolResult<()> {
        let (_temporary, source, output) = fixture()?;
        sample(&source, "pair001a", true)?;
        common::write_json(
            &source.join(RECEIPT),
            &json!({
                "schema_version": 1, "spawned": false, "process_group": null,
                "cleanup_complete": true, "exit_code": 127, "timed_out": false,
            }),
        )?;
        assert!(seal(&source, &output).is_err());
        assert_failure_only(&output)?;
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn failed_copy_discards_already_staged_raw() -> ToolResult<()> {
        use std::os::unix::fs::symlink;
        let (_temporary, source, output) = fixture()?;
        sample(&source, "pair001a", true)?;
        let bad = source.join("raw/pair001b");
        fs::create_dir_all(&bad)?;
        symlink("/outside", bad.join("unsafe-profile"))?;
        let publication = Publication::prepare(&source, &output)?;
        let stage = publication.stage()?;
        assert!(fill(&publication, stage.path()).is_err());
        assert!(stage.path().join("raw/pair001a/sample.json").is_file());
        stage.close()?;
        assert!(seal(&source, &output).is_err());
        assert_failure_only(&output)?;
        assert!(source.join("raw/pair001a/sample.json").is_file());
        assert!(fs::symlink_metadata(bad.join("unsafe-profile"))?.is_symlink());
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn special_files_are_rejected_without_opening_them() -> ToolResult<()> {
        use nix::{sys::stat::Mode, unistd::mkfifo};
        let (_temporary, source, output) = fixture()?;
        let sample = sample(&source, "pair001a", true)?;
        mkfifo(&sample.join("writer-pipe"), Mode::S_IRUSR | Mode::S_IWUSR)?;
        assert!(seal(&source, &output).is_err());
        assert_failure_only(&output)?;
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn invalid_lifecycle_receipts_never_publish_raw() -> ToolResult<()> {
        for replacement in [
            json!({"schema_version": 2}),
            json!({"process_group": null}),
            json!({"process_group": 0}),
            json!({"spawned": false}),
            json!({"exit_code": null}),
        ] {
            let (_temporary, source, output) = fixture()?;
            sample(&source, "pair001a", true)?;
            let mut receipt = read_json(&source.join(RECEIPT))?;
            for (name, value) in replacement.as_object().ok_or("Invalid test replacement")? {
                receipt[name] = value.clone();
            }
            common::write_json(&source.join(RECEIPT), &receipt)?;
            assert!(seal(&source, &output).is_err());
            assert_failure_only(&output)?;
        }
        let (_temporary, source, output) = fixture()?;
        fs::remove_file(source.join(RECEIPT))?;
        assert!(seal(&source, &output).is_err());
        assert_failure_only(&output)?;
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn symlink_source_and_destination_are_not_followed() -> ToolResult<()> {
        use std::os::unix::fs::symlink;
        let (temporary, source, output) = fixture()?;
        sample(&source, "pair001a", true)?;
        let source_link = temporary.path().join("source-link");
        symlink(&source, &source_link)?;
        assert!(seal(&source_link, &output).is_err());
        assert_failure_only(&output)?;
        let destination_link = temporary.path().join("destination-link");
        let untouched_target = temporary.path().join("missing-target");
        symlink(&untouched_target, &destination_link)?;
        assert!(seal(&source, &destination_link).is_err());
        assert!(!untouched_target.exists());
        assert!(source.join("raw/pair001a/sample.json").is_file());
        assert!(seal(&source, &source_link.join("nested")).is_err());
        assert!(!source.join("nested").exists());
        Ok(())
    }

    #[test]
    fn source_overlap_and_existing_destinations_are_never_modified() -> ToolResult<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("keep"), b"untouched")?;
        assert!(seal(&source, &source.join("nested")).is_err());
        assert!(seal(&source, &source).is_err());
        assert!(seal(&source, temporary.path()).is_err());
        let output = temporary.path().join("existing");
        fs::create_dir(&output)?;
        fs::write(output.join("keep"), b"also untouched")?;
        assert!(seal(&source, &output).is_err());
        assert_eq!(fs::read(source.join("keep"))?, b"untouched");
        assert_eq!(fs::read(output.join("keep"))?, b"also untouched");
        assert!(!source.join("nested").exists());
        Ok(())
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn atomic_publication_refuses_a_destination_created_after_staging() -> ToolResult<()> {
        let (_temporary, source, output) = fixture()?;
        let publication = Publication::prepare(&source, &output)?;
        let stage = publication.stage()?;
        sample(&source, "pair001a", true)?;
        fill(&publication, stage.path())?;
        fs::create_dir(&output)?;
        assert!(rename_new(&publication.parent, stage.path(), &output).is_err());
        assert!(fs::read_dir(&output)?.next().is_none());
        assert!(stage.path().join("raw/pair001a/sample.json").is_file());
        Ok(())
    }

    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    #[test]
    fn unsupported_atomic_publication_fails_without_a_destination() -> ToolResult<()> {
        let temporary = tempfile::tempdir()?;
        let source = temporary.path().join("source");
        let output = temporary.path().join("published");
        fs::create_dir(&source)?;
        assert!(seal(&source, &output).is_err());
        assert!(!output.exists());
        assert!(fs::read_dir(source)?.next().is_none());
        Ok(())
    }
}
