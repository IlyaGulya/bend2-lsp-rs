use super::{
    ci_metadata, command_line, command_output, executable_identity, hardware_metadata, identity,
    read_json, revision, root, verify_identity,
};
use crate::{ToolResult, common};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

mod heap;
mod lifecycle;
mod profiles;

const APPLICATIONS: [&str; 3] = ["bend2-lsp", "line_index_profile", "folding_allocations"];
const LINE_INDEX_MODES: [&str; 6] = [
    "position-ascii",
    "position-unicode",
    "snapshot-small",
    "snapshot-medium",
    "snapshot-large",
    "snapshot-medium-unicode",
];

#[derive(Parser)]
#[command(no_binary_name = true)]
struct Args {
    #[command(subcommand)]
    command: Mode,
}

#[derive(Subcommand)]
enum Mode {
    Provenance {
        #[arg(long, value_parser = revision)]
        baseline_revision: String,
        #[arg(long, value_parser = revision)]
        candidate_revision: String,
        #[arg(long)]
        commands_file: PathBuf,
        #[arg(long)]
        binary_dir: Vec<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
    Collect {
        #[arg(long)]
        binary_dir: PathBuf,
        #[arg(long)]
        provenance: PathBuf,
        #[arg(long)]
        output_dir: PathBuf,
    },
}

pub(super) fn run(arguments: &[String]) -> ToolResult<()> {
    let args = Args::try_parse_from(arguments)?;
    common::require_ci()?;
    match args.command {
        Mode::Provenance {
            baseline_revision,
            candidate_revision,
            commands_file,
            binary_dir,
            output,
        } => provenance(
            &baseline_revision,
            &candidate_revision,
            &commands_file,
            &binary_dir,
            &output,
        ),
        Mode::Collect {
            binary_dir,
            provenance,
            output_dir,
        } => profiles::collect(&binary_dir, &provenance, &output_dir),
    }
}

fn visit_files(directory: &Path, files: &mut Vec<PathBuf>) -> ToolResult<()> {
    if !directory.is_dir() {
        return Err(format!("Not a binary/source directory: {}", directory.display()).into());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(format!(
                "Symbol/input links must be materialized: {}",
                entry.path().display()
            )
            .into());
        }
        if kind.is_dir() {
            visit_files(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn source_inputs() -> ToolResult<Vec<Value>> {
    let root = root();
    let mut paths: Vec<_> = [
        "Cargo.toml",
        "Cargo.lock",
        "src/main.rs",
        "examples/line_index_profile.rs",
        "examples/folding_allocations.rs",
        "tools/perf/Cargo.toml",
        ".github/workflows/performance.yml",
        "benches/fixtures/analyzer_input.bend",
        "benches/fixtures/analyzer_medium.bend",
        "benches/fixtures/analyzer_large.bend",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .collect();
    let mut rust_sources = Vec::new();
    visit_files(&root.join("tools/perf/src"), &mut rust_sources)?;
    paths.extend(
        rust_sources
            .into_iter()
            .filter(|path| path.extension().is_some_and(|extension| extension == "rs")),
    );
    paths.sort();
    paths.iter().map(|path| identity(path)).collect()
}

fn provenance(
    baseline_revision: &str,
    candidate_revision: &str,
    commands_file: &Path,
    directories: &[PathBuf],
    output: &Path,
) -> ToolResult<()> {
    let target = common::native_target()?;
    let rustc = command_output(&["rustc", "-vV"])?;
    let host = rustc.lines().find_map(|line| line.strip_prefix("host: "));
    if host != Some(target) {
        return Err(format!("Rust host {host:?} is not native target {target}").into());
    }
    if command_output(&["git", "rev-parse", "HEAD"])? != candidate_revision {
        return Err("Candidate checkout differs from declared revision".into());
    }
    let commands = fs::read_to_string(commands_file)?;
    if commands.trim().is_empty() {
        return Err("Build and collection command provenance is empty".into());
    }
    let mut files = Vec::new();
    for directory in directories {
        visit_files(directory, &mut files)?;
    }
    files.sort();
    if files.is_empty() {
        return Err("Build provenance contains no binaries or symbol files".into());
    }
    let binaries: BTreeMap<_, _> = files
        .iter()
        .map(|path| -> ToolResult<_> {
            let item = identity(path)?;
            Ok((
                item["path"]
                    .as_str()
                    .ok_or("Invalid binary identity path")?
                    .to_owned(),
                item,
            ))
        })
        .collect::<ToolResult<_>>()?;
    let build_environment: BTreeMap<_, _> = [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTC_WRAPPER",
        "CARGO_BUILD_TARGET",
        "CARGO_PROFILE_RELEASE_OPT_LEVEL",
        "CARGO_PROFILE_RELEASE_LTO",
        "CARGO_PROFILE_RELEASE_CODEGEN_UNITS",
        "CARGO_PROFILE_RELEASE_DEBUG",
        "CARGO_PROFILE_RELEASE_STRIP",
        "CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO",
    ]
    .into_iter()
    .map(|key| (key, env::var(key).ok()))
    .collect();
    let data = json!({
        "format_version": 1, "native_target": target,
        "source": {
            "baseline_revision": baseline_revision, "candidate_revision": candidate_revision,
            "baseline_tree": command_output(&["git", "rev-parse", &format!("{baseline_revision}^{{tree}}")])?,
            "candidate_tree": command_output(&["git", "rev-parse", &format!("{candidate_revision}^{{tree}}")])?,
            "candidate_revision_kind": "workflow PR merge commit", "inputs": source_inputs()?,
        },
        "toolchain": {"rustc": rustc, "cargo": command_output(&["cargo", "-V"])?, "rustup": command_output(&["rustup", "show", "active-toolchain"])?},
        "build_environment": build_environment, "hardware": hardware_metadata(), "harness": executable_identity()?,
        "build_command_environment": build_command_environment(&commands),
        "ci": ci_metadata(), "commands": commands, "commands_file": identity(commands_file)?,
        "provenance_command": command_line(), "binary_and_symbol_files": binaries,
        "allocation_profile": "candidate only; separate release opt-level=3/lto=fat/codegen-units=1 with dhat-heap, debug=1, strip=none; report-only; never timing evidence"
    });
    common::write_json(output, &data)
}

fn verify_runner(data: &Value, target: &str, current_ci: &Value) -> ToolResult<()> {
    if data["format_version"].as_u64() != Some(1) || data["native_target"] != target {
        return Err("Profile/build format or native targets differ".into());
    }
    for key in [
        "GITHUB_RUN_ID",
        "GITHUB_RUN_ATTEMPT",
        "GITHUB_SHA",
        "GITHUB_JOB",
        "RUNNER_OS",
        "RUNNER_ARCH",
        "RUNNER_NAME",
    ] {
        if data["ci"][key] != current_ci[key] {
            return Err(format!("Profile and build same-runner provenance differs: {key}").into());
        }
    }
    Ok(())
}

fn build_command_environment(commands: &str) -> BTreeMap<&str, &str> {
    commands
        .split_whitespace()
        .filter_map(|token| token.split_once('='))
        .filter(|(key, _)| key.starts_with("CARGO_PROFILE_RELEASE_"))
        .collect()
}

fn verify_provenance(data: &Value, target: &str) -> ToolResult<()> {
    verify_runner(data, target, &ci_metadata())?;
    let candidate = data["source"]["candidate_revision"]
        .as_str()
        .ok_or("Missing candidate revision")?;
    revision(candidate)?;
    revision(
        data["source"]["baseline_revision"]
            .as_str()
            .ok_or("Missing baseline revision")?,
    )?;
    if command_output(&["git", "rev-parse", "HEAD"])? != candidate {
        return Err("Profile checkout differs from declared candidate revision".into());
    }
    verify_identity(&data["harness"])?;
    let inputs = data["source"]["inputs"]
        .as_array()
        .ok_or("Missing source identities")?;
    if inputs.is_empty() {
        return Err("Empty source provenance".into());
    }
    if *inputs != source_inputs()? {
        return Err("Source identities differ from current complete Rust harness inputs".into());
    }
    for item in inputs {
        verify_identity(item)?;
    }
    let files = data["binary_and_symbol_files"]
        .as_object()
        .ok_or("Missing binary/symbol identities")?;
    if files.is_empty() {
        return Err("Empty binary/symbol provenance".into());
    }
    for (key, item) in files {
        if item["path"] != *key {
            return Err("Binary identity key differs from path".into());
        }
        verify_identity(item)?;
    }
    Ok(())
}

fn verify_debug_sidecars(
    binary: &Path,
    files: &serde_json::Map<String, Value>,
    packed: bool,
) -> ToolResult<()> {
    let stem = binary
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or("Binary missing Unicode file stem")?;
    let parent = binary.parent().ok_or("Binary missing parent")?;
    let normalized = stem.replace('-', "_");
    let direct_pdb = parent.join(stem).with_extension("pdb");
    let pdb = if direct_pdb.exists() {
        direct_pdb
    } else {
        parent.join(&normalized).with_extension("pdb")
    };
    if cfg!(target_os = "windows") && !pdb.is_file() {
        return Err(format!("Missing native PDB debug sidecar: {}", pdb.display()).into());
    }
    if pdb.is_file() {
        let key = pdb.canonicalize()?;
        let item = files
            .get(key.to_str().ok_or("Non-Unicode debug path")?)
            .ok_or("PDB missing from build provenance")?;
        verify_identity(item)?;
        let mut bytes = [0_u8; 32];
        std::io::Read::read_exact(&mut fs::File::open(&pdb)?, &mut bytes)?;
        if bytes != *b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0" {
            return Err("Invalid PDB debug sidecar format".into());
        }
    }
    let direct_symbols = parent.join(format!("{stem}.dSYM"));
    let symbols = if direct_symbols.exists() {
        direct_symbols
    } else {
        parent.join(format!("{normalized}.dSYM"))
    };
    if cfg!(target_os = "macos") && packed && !symbols.is_dir() {
        return Err(format!(
            "Missing packed native dSYM debug sidecar: {}",
            symbols.display()
        )
        .into());
    }
    if symbols.exists() {
        verify_dsym(&symbols, stem, &normalized, files)?;
    }
    Ok(())
}

fn verify_dsym(
    symbols: &Path,
    stem: &str,
    normalized: &str,
    files: &serde_json::Map<String, Value>,
) -> ToolResult<()> {
    let contents = symbols.join("Contents");
    let resources = contents.join("Resources");
    let dwarf_root = resources.join("DWARF");
    for directory in [symbols, &contents, &resources, &dwarf_root] {
        if !fs::symlink_metadata(directory)?.is_dir() {
            return Err(format!(
                "Malformed or unmaterialized dSYM directory: {}",
                directory.display()
            )
            .into());
        }
    }
    let mut members = fs::read_dir(&dwarf_root)?;
    let member = members.next().ok_or("Missing dSYM DWARF member")??;
    if members.next().is_some() {
        return Err("Native executable dSYM must contain exactly one DWARF member".into());
    }
    let name = member.file_name();
    let name = name.to_str().ok_or("Non-Unicode dSYM DWARF member")?;
    // Cargo uplifts the bundle name, not its contents. dsymutil retains the
    // original crate-name[-UnitHash] input basename (UnitHash is 16 lower hex).
    let cargo_hash = name
        .strip_prefix(normalized)
        .and_then(|suffix| suffix.strip_prefix('-'))
        .is_some_and(|hash| {
            hash.len() == 16
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        });
    if name != stem && name != normalized && !cargo_hash {
        return Err(format!("Unrelated or unknown dSYM DWARF member: {name}").into());
    }
    for path in [member.path(), contents.join("Info.plist")] {
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(format!(
                "Malformed or unmaterialized dSYM component: {}",
                path.display()
            )
            .into());
        }
        let key = path.canonicalize()?;
        let item = files
            .get(key.to_str().ok_or("Non-Unicode debug path")?)
            .ok_or("dSYM component missing from provenance")?;
        if identity(&path)? != *item {
            return Err(format!("dSYM component identity changed: {}", path.display()).into());
        }
    }
    Ok(())
}

fn jobs() -> Vec<(String, &'static str, Vec<String>)> {
    let mut jobs = vec![("lsp-lifecycle".to_owned(), "bend2-lsp", Vec::new())];
    jobs.extend(LINE_INDEX_MODES.into_iter().map(|mode| {
        (
            format!("line-index-{mode}"),
            "line_index_profile",
            vec![mode.to_owned()],
        )
    }));
    for lines in [100, 1000, 10000] {
        for mode in ["snapshot", "fold"] {
            let mut args = vec![lines.to_string()];
            if mode == "fold" {
                args.push("fold".to_owned());
            }
            jobs.push((
                format!("folding-{lines}-{mode}"),
                "folding_allocations",
                args,
            ));
        }
    }
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_job_contract_is_exactly_candidate_only_thirteen() {
        let jobs = jobs();
        assert_eq!(jobs.len(), 13);
        assert_eq!(
            jobs[0],
            (
                "lsp-lifecycle".to_owned(),
                "bend2-lsp",
                Vec::<String>::new()
            )
        );
        assert_eq!(jobs[1].2, ["position-ascii"]);
        assert_eq!(jobs[6].2, ["snapshot-medium-unicode"]);
        assert_eq!(
            jobs[7],
            (
                "folding-100-snapshot".to_owned(),
                "folding_allocations",
                vec!["100".to_owned()]
            )
        );
        assert_eq!(
            jobs[12],
            (
                "folding-10000-fold".to_owned(),
                "folding_allocations",
                vec!["10000".to_owned(), "fold".to_owned()]
            )
        );
        assert_eq!(
            jobs.iter()
                .filter(|(_, binary, _)| *binary == "line_index_profile")
                .count(),
            6
        );
        assert_eq!(
            jobs.iter()
                .filter(|(_, binary, _)| *binary == "folding_allocations")
                .count(),
            6
        );
    }

    #[test]
    fn memory_cli_has_only_original_commands() {
        assert!(
            Args::try_parse_from([
                "collect",
                "--binary-dir",
                "bin",
                "--provenance",
                "provenance.json",
                "--output-dir",
                "memory"
            ])
            .is_ok()
        );
        assert!(
            Args::try_parse_from([
                "provenance",
                "--baseline-revision",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "--candidate-revision",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "--commands-file",
                "commands.txt",
                "--binary-dir",
                "timing",
                "--binary-dir",
                "memory",
                "--output",
                "provenance.json"
            ])
            .is_ok()
        );
        assert!(Args::try_parse_from(["collect", "--binary-dir", "bin"]).is_err());
        assert!(Args::try_parse_from(["stamp"]).is_err());
        assert!(Args::try_parse_from(["verify"]).is_err());
    }

    #[test]
    fn same_runner_provenance_cannot_mix_run_attempt_target_or_host() -> ToolResult<()> {
        let ci = json!({"GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "1", "GITHUB_SHA": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "GITHUB_JOB": "native", "RUNNER_OS": "Linux", "RUNNER_ARCH": "ARM64", "RUNNER_NAME": "host-1"});
        let data =
            json!({"format_version": 1, "native_target": "aarch64-unknown-linux-gnu", "ci": ci});
        verify_runner(&data, "aarch64-unknown-linux-gnu", &ci)?;
        for key in [
            "GITHUB_RUN_ID",
            "GITHUB_RUN_ATTEMPT",
            "GITHUB_SHA",
            "GITHUB_JOB",
            "RUNNER_OS",
            "RUNNER_ARCH",
            "RUNNER_NAME",
        ] {
            let mut other = ci.clone();
            other[key] = json!("different");
            assert!(
                verify_runner(&data, "aarch64-unknown-linux-gnu", &other).is_err(),
                "Mixed {key}"
            );
        }
        assert!(verify_runner(&data, "x86_64-unknown-linux-gnu", &ci).is_err());
        Ok(())
    }

    #[test]
    fn declared_debug_build_overrides_are_preserved() {
        let values = build_command_environment(
            "env CARGO_PROFILE_RELEASE_DEBUG=1 CARGO_PROFILE_RELEASE_STRIP=none CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed cargo build --release --features dhat-heap",
        );
        assert_eq!(values["CARGO_PROFILE_RELEASE_DEBUG"], "1");
        assert_eq!(values["CARGO_PROFILE_RELEASE_STRIP"], "none");
        assert_eq!(values["CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO"], "packed");
    }

    #[test]
    fn symbol_sidecars_require_recorded_identity_and_valid_structure() -> ToolResult<()> {
        let temporary = tempfile::tempdir()?;
        let suffix = if cfg!(target_os = "windows") {
            ".exe"
        } else {
            ""
        };
        let binary = temporary.path().join(format!("bend2-lsp{suffix}"));
        fs::write(&binary, b"binary fixture")?;
        let pdb = temporary.path().join("bend2-lsp.pdb");
        fs::write(&pdb, b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0")?;
        let item = identity(&pdb)?;
        let mut files = serde_json::Map::new();
        files.insert(
            item["path"].as_str().ok_or("Missing path")?.to_owned(),
            item,
        );
        verify_debug_sidecars(&binary, &files, false)?;
        assert!(verify_debug_sidecars(&binary, &serde_json::Map::new(), false).is_err());
        fs::write(&pdb, b"malformed PDB")?;
        let item = identity(&pdb)?;
        files.insert(
            item["path"].as_str().ok_or("Missing path")?.to_owned(),
            item,
        );
        assert!(verify_debug_sidecars(&binary, &files, false).is_err());
        Ok(())
    }

    fn dsym_fixture(
        directory: &Path,
        stem: &str,
        member: &str,
    ) -> ToolResult<(PathBuf, serde_json::Map<String, Value>)> {
        let suffix = if cfg!(target_os = "windows") {
            ".exe"
        } else {
            ""
        };
        let binary = directory.join(format!("{stem}{suffix}"));
        fs::write(&binary, b"binary fixture")?;
        fs::write(
            directory.join(format!("{stem}.pdb")),
            b"Microsoft C/C++ MSF 7.00\r\n\x1aDS\0\0\0",
        )?;
        let contents = directory.join(format!("{stem}.dSYM/Contents"));
        let dwarf_root = contents.join("Resources/DWARF");
        fs::create_dir_all(&dwarf_root)?;
        fs::write(contents.join("Info.plist"), b"<plist><dict/></plist>")?;
        fs::write(dwarf_root.join(member), b"DWARF fixture")?;
        let mut paths = Vec::new();
        visit_files(directory, &mut paths)?;
        let files = paths
            .iter()
            .map(|path| -> ToolResult<_> {
                let item = identity(path)?;
                Ok((
                    item["path"].as_str().ok_or("Missing path")?.to_owned(),
                    item,
                ))
            })
            .collect::<ToolResult<_>>()?;
        Ok((binary, files))
    }

    #[test]
    fn cargo_dsym_sidecars_accept_materialized_binary_and_example_members() -> ToolResult<()> {
        for (stem, member) in [
            ("bend2-lsp", "bend2-lsp"),
            ("bend2-lsp", "bend2_lsp"),
            ("bend2-lsp", "bend2_lsp-0123456789abcdef"),
            ("line_index_profile", "line_index_profile-0123456789abcdef"),
            (
                "folding_allocations",
                "folding_allocations-fedcba9876543210",
            ),
        ] {
            let temporary = tempfile::tempdir()?;
            let (binary, files) = dsym_fixture(temporary.path(), stem, member)?;
            verify_debug_sidecars(&binary, &files, true)?;
        }
        Ok(())
    }

    #[test]
    fn cargo_dsym_sidecars_reject_unknown_or_ambiguous_members() -> ToolResult<()> {
        for member in [
            "unrelated-0123456789abcdef",
            "line_index_profile_backup-0123456789abcdef",
            "line_index_profile-not-a-cargo-hash",
            "line_index_profile-0123456789abcde",
            "line_index_profile-0123456789abcdef0",
            "line_index_profile-0123456789abcdeg",
            "line_index_profile-0123456789ABCDEf",
        ] {
            let temporary = tempfile::tempdir()?;
            let (binary, files) = dsym_fixture(temporary.path(), "line_index_profile", member)?;
            assert!(
                verify_debug_sidecars(&binary, &files, true).is_err(),
                "{member}"
            );
        }
        let temporary = tempfile::tempdir()?;
        let (binary, files) = dsym_fixture(
            temporary.path(),
            "line_index_profile",
            "line_index_profile-0123456789abcdef",
        )?;
        fs::write(
            temporary
                .path()
                .join("line_index_profile.dSYM/Contents/Resources/DWARF/line_index_profile"),
            b"second DWARF member",
        )?;
        assert!(verify_debug_sidecars(&binary, &files, true).is_err());
        Ok(())
    }

    #[test]
    fn cargo_dsym_sidecars_require_exact_identities_and_nonempty_components() -> ToolResult<()> {
        let stem = "line_index_profile";
        let member = "line_index_profile-0123456789abcdef";
        for component in [
            "Contents/Info.plist",
            "Contents/Resources/DWARF/line_index_profile-0123456789abcdef",
        ] {
            let temporary = tempfile::tempdir()?;
            let (binary, files) = dsym_fixture(temporary.path(), stem, member)?;
            let symbols = temporary.path().join(format!("{stem}.dSYM"));
            let path = symbols.join(component);
            let key = path.canonicalize()?;
            let key = key.to_str().ok_or("Missing component path")?;
            let mut missing = files.clone();
            missing.remove(key);
            assert!(verify_dsym(&symbols, stem, stem, &missing).is_err());
            let mut unrelated = files.clone();
            unrelated.insert(key.to_owned(), identity(&binary)?);
            assert!(verify_dsym(&symbols, stem, stem, &unrelated).is_err());
            fs::write(&path, b"changed component")?;
            assert!(verify_dsym(&symbols, stem, stem, &files).is_err());
            fs::write(&path, b"")?;
            let mut empty = files.clone();
            empty.insert(key.to_owned(), identity(&path)?);
            assert!(verify_dsym(&symbols, stem, stem, &empty).is_err());
            fs::remove_file(&path)?;
            assert!(verify_dsym(&symbols, stem, stem, &files).is_err());
            fs::create_dir(&path)?;
            assert!(verify_dsym(&symbols, stem, stem, &files).is_err());
        }
        let temporary = tempfile::tempdir()?;
        assert!(
            verify_dsym(
                &temporary.path().join("absent.dSYM"),
                stem,
                stem,
                &serde_json::Map::new()
            )
            .is_err()
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn cargo_dsym_sidecars_reject_unmaterialized_bundle_and_member_links() -> ToolResult<()> {
        let temporary = tempfile::tempdir()?;
        let stem = "line_index_profile";
        let member = "line_index_profile-0123456789abcdef";
        let (binary, files) = dsym_fixture(temporary.path(), stem, member)?;
        let symbols = temporary.path().join(format!("{stem}.dSYM"));
        let hashed_symbols = temporary.path().join(format!("{member}.dSYM"));
        fs::rename(&symbols, &hashed_symbols)?;
        std::os::unix::fs::symlink(&hashed_symbols, &symbols)?;
        assert!(verify_debug_sidecars(&binary, &files, true).is_err());
        assert!(visit_files(temporary.path(), &mut Vec::new()).is_err());
        fs::remove_file(&symbols)?;
        fs::rename(&hashed_symbols, &symbols)?;
        let dwarf = symbols.join("Contents/Resources/DWARF").join(member);
        let target = temporary.path().join("linked-dwarf");
        fs::rename(&dwarf, &target)?;
        std::os::unix::fs::symlink(&target, &dwarf)?;
        assert!(verify_debug_sidecars(&binary, &files, true).is_err());
        Ok(())
    }
}
