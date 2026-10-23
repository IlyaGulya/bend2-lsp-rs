use crate::ToolResult;
use bincode::Options as _;
use iai_callgrind_runner::api::LibraryBenchmarkGroups;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::Path,
};

use super::{Identity, require};

pub(crate) const CAPTURE_DIRECTORY: &str = "BEND2_PERF_IAI_CAPTURE_DIRECTORY";
const MAX_BYTES: usize = 8 * 1024 * 1024;
const RUNNER_NAME: &str = "iai-callgrind-runner";

pub(crate) struct FunctionDescriptor {
    pub label: String,
    pub bytes: Vec<u8>,
    pub identities: BTreeSet<Identity>,
}

pub(crate) struct DescriptorPlan {
    pub runner_arguments: Vec<OsString>,
    pub functions: Vec<FunctionDescriptor>,
}

/// The private PATH shim is recognized before the ordinary performance CLI is parsed.
/// Capture files are original evidence; selection never overwrites either file.
pub(crate) fn capture_if_requested() -> ToolResult<bool> {
    let mut arguments = std::env::args_os();
    if arguments
        .next()
        .as_deref()
        .and_then(|arg| Path::new(arg).file_name())
        != Some(std::ffi::OsStr::new(RUNNER_NAME))
    {
        return Ok(false);
    }
    let directory = std::env::var_os(CAPTURE_DIRECTORY)
        .filter(|value| !value.is_empty())
        .ok_or("iai descriptor capture directory is required in shim mode")?;
    let arguments = arguments
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "iai runner argument is not UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    capture(
        Path::new(&directory),
        &arguments,
        &mut std::io::stdin().lock(),
    )?;
    Ok(true)
}

fn framing(arguments: &[String]) -> ToolResult<usize> {
    require(
        arguments.len() == 8,
        "iai runner requires exactly eight original arguments",
    )?;
    require(arguments[0] == "0.16.1", "unsupported iai runner version")?;
    require(
        arguments[1] == "--lib-bench",
        "only iai library benchmarks can be partitioned",
    )?;
    require(
        arguments
            .iter()
            .all(|arg| !arg.is_empty() && !arg.contains('\0')),
        "iai runner arguments must be nonempty and contain no NUL",
    )?;
    let length = arguments[7].parse::<usize>()?;
    require(
        length > 0 && length <= MAX_BYTES && arguments[7] == length.to_string(),
        "iai descriptor byte count must be canonical and within 8 MiB",
    )?;
    Ok(length)
}

fn bounded_read(input: &mut impl Read) -> ToolResult<Vec<u8>> {
    let mut bytes = Vec::new();
    input.take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes)?;
    require(bytes.len() <= MAX_BYTES, "iai capture exceeds 8 MiB")?;
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> ToolResult<LibraryBenchmarkGroups> {
    require(
        !bytes.is_empty() && bytes.len() <= MAX_BYTES,
        "invalid iai descriptor size",
    )?;
    // bincode::serialize uses fixed-width encoding. Unlike its convenience decoder,
    // these options reject trailing data and bound decoding as well as the read.
    let descriptor: LibraryBenchmarkGroups = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_BYTES as u64)
        .reject_trailing_bytes()
        .deserialize(bytes)?;
    require(
        bincode::serialize(&descriptor)? == bytes,
        "iai descriptor does not roundtrip exactly through pinned API",
    )?;
    Ok(descriptor)
}

fn capture(directory: &Path, arguments: &[String], input: &mut impl Read) -> ToolResult<()> {
    let length = framing(arguments)?;
    let argv = serde_json::to_vec_pretty(arguments)?;
    require(
        argv.len() <= MAX_BYTES,
        "iai captured arguments exceed 8 MiB",
    )?;
    let bytes = bounded_read(input)?;
    require(
        bytes.len() == length,
        "iai descriptor byte count does not match stdin",
    )?;
    decode(&bytes)?;
    // An existing directory, even empty, must not be reused as capture evidence.
    fs::create_dir(directory)?;
    let mut descriptor = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("descriptor.bin"))?;
    descriptor.write_all(&bytes)?;
    let mut argv_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join("argv.json"))?;
    argv_file.write_all(&argv)?;
    Ok(())
}

pub(crate) fn plan(
    directory: &Path,
    expected: &BTreeSet<Identity>,
    executable: &Path,
) -> ToolResult<DescriptorPlan> {
    let argv = bounded_read(&mut File::open(directory.join("argv.json"))?)?;
    let arguments: Vec<String> = serde_json::from_slice(&argv)?;
    let length = framing(&arguments)?;
    require(
        Path::new(&arguments[6]) == executable,
        "iai captured executable does not match frozen executable",
    )?;
    let bytes = bounded_read(&mut File::open(directory.join("descriptor.bin"))?)?;
    require(
        bytes.len() == length,
        "iai descriptor byte count does not match argv.json",
    )?;
    let mut descriptor = decode(&bytes)?;
    require(
        !descriptor.has_setup && !descriptor.has_teardown,
        "iai main setup/teardown cannot be partitioned",
    )?;
    require(
        !expected.is_empty(),
        "iai expected workload set must be nonempty",
    )?;

    let mut seen = BTreeSet::new();
    let mut selected = Vec::new();
    for (group_index, group) in descriptor.groups.iter_mut().enumerate() {
        require(
            !group.has_setup && !group.has_teardown,
            "iai group setup/teardown cannot be partitioned",
        )?;
        require(
            group.compare_by_id != Some(true),
            "iai cross-function comparisons cannot be partitioned",
        )?;
        for (function_index, function) in group.library_benchmarks.iter_mut().enumerate() {
            if function.benches.is_empty() {
                continue;
            }
            let name = &function.benches[0].function_name;
            let mut identities = BTreeSet::new();
            for bench in &function.benches {
                require(
                    !name.trim().is_empty()
                        && bench.function_name == *name
                        && bench.id.as_ref().is_none_or(|id| !id.trim().is_empty()),
                    "iai function slot has invalid or mixed workload identities",
                )?;
                let identity = (bench.function_name.clone(), bench.id.clone());
                require(
                    seen.insert(identity.clone()),
                    "duplicate iai descriptor workload identity",
                )?;
                identities.insert(identity);
            }
            // Move case payloads out once. The unmodified outer structure retains
            // every group/function index, config, default tool and command-line arg.
            selected.push((
                group_index,
                function_index,
                identities,
                std::mem::take(&mut function.benches),
            ));
        }
    }
    require(
        &seen == expected,
        "iai descriptor workload set does not match frozen --list",
    )?;

    let mut functions = Vec::with_capacity(selected.len());
    for (group_index, function_index, identities, benches) in selected {
        descriptor.groups[group_index].library_benchmarks[function_index].benches = benches;
        let bytes = bincode::serialize(&descriptor)?;
        // Drop only the selected payload after encoding; the next function's cases
        // are still owned by `selected`, not deep-cloned from the original suite.
        descriptor.groups[group_index].library_benchmarks[function_index]
            .benches
            .clear();
        functions.push(FunctionDescriptor {
            label: format!("group{group_index}-function{function_index}"),
            bytes,
            identities,
        });
    }
    Ok(DescriptorPlan {
        runner_arguments: arguments.into_iter().map(OsString::from).collect(),
        functions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iai_callgrind_runner::api::{
        LibraryBenchmark, LibraryBenchmarkBench, LibraryBenchmarkConfig, LibraryBenchmarkGroup,
        ValgrindTool,
    };
    use std::io::Cursor;

    fn bench(name: &str, id: Option<&str>) -> LibraryBenchmarkBench {
        LibraryBenchmarkBench {
            function_name: name.to_owned(),
            id: id.map(str::to_owned),
            args: Some("original argument representation".to_owned()),
            config: Some(LibraryBenchmarkConfig {
                env_clear: Some(false),
                envs: vec![("ORIGINAL_CASE".into(), Some("unchanged".into()))],
                ..Default::default()
            }),
        }
    }

    fn fixture() -> LibraryBenchmarkGroups {
        LibraryBenchmarkGroups {
            command_line_args: vec!["frozen-benchmark".into(), "--output-format=json".into()],
            config: LibraryBenchmarkConfig {
                env_clear: Some(false),
                envs: vec![("ORIGINAL_SUITE".into(), None)],
                ..Default::default()
            },
            default_tool: ValgrindTool::Callgrind,
            groups: vec![
                LibraryBenchmarkGroup::default(),
                LibraryBenchmarkGroup {
                    id: "original_group".into(),
                    compare_by_id: Some(false),
                    config: Some(LibraryBenchmarkConfig::default()),
                    library_benchmarks: vec![
                        LibraryBenchmark::default(),
                        LibraryBenchmark {
                            benches: vec![
                                bench("first", Some("small")),
                                bench("first", Some("large")),
                            ],
                            config: Some(LibraryBenchmarkConfig::default()),
                        },
                        LibraryBenchmark {
                            benches: vec![bench("second", None)],
                            config: None,
                        },
                    ],
                    ..Default::default()
                },
            ],
            has_setup: false,
            has_teardown: false,
        }
    }

    fn identities(descriptor: &LibraryBenchmarkGroups) -> BTreeSet<Identity> {
        descriptor
            .groups
            .iter()
            .flat_map(|group| &group.library_benchmarks)
            .flat_map(|function| &function.benches)
            .map(|bench| (bench.function_name.clone(), bench.id.clone()))
            .collect()
    }

    fn arguments(length: usize) -> Vec<String> {
        [
            "0.16.1",
            "--lib-bench",
            "/original/package",
            "original-package",
            "benches/analysis.rs",
            "analysis",
            "/frozen/analysis",
        ]
        .into_iter()
        .map(str::to_owned)
        .chain([length.to_string()])
        .collect()
    }

    fn evidence(
        descriptor: &LibraryBenchmarkGroups,
    ) -> ToolResult<(tempfile::TempDir, std::path::PathBuf)> {
        let temporary = tempfile::tempdir()?;
        let directory = temporary.path().join("capture");
        let bytes = bincode::serialize(descriptor)?;
        capture(&directory, &arguments(bytes.len()), &mut Cursor::new(bytes))?;
        Ok((temporary, directory))
    }

    #[test]
    fn partitions_every_case_without_changing_dispatch_indices_or_configuration() -> ToolResult<()>
    {
        let original = fixture();
        let expected = identities(&original);
        let (_temporary, directory) = evidence(&original)?;
        let original_bytes = fs::read(directory.join("descriptor.bin"))?;
        let original_argv = fs::read(directory.join("argv.json"))?;
        let plan = plan(&directory, &expected, Path::new("/frozen/analysis"))?;
        assert_eq!(
            plan.runner_arguments,
            arguments(original_bytes.len())
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(plan.functions.len(), 2);
        let mut partition = BTreeSet::new();
        for (selected, function) in [1, 2].into_iter().zip(&plan.functions) {
            assert_eq!(function.label, format!("group1-function{selected}"));
            let actual = decode(&function.bytes)?;
            let mut wanted = original.clone();
            for (index, entry) in wanted.groups[1].library_benchmarks.iter_mut().enumerate() {
                if index != selected {
                    entry.benches.clear();
                }
            }
            assert_eq!(actual, wanted);
            assert_eq!(function.identities, identities(&actual));
            for identity in &function.identities {
                assert!(partition.insert(identity.clone()));
            }
        }
        assert_eq!(partition, expected);
        assert_eq!(fs::read(directory.join("descriptor.bin"))?, original_bytes);
        assert_eq!(fs::read(directory.join("argv.json"))?, original_argv);
        Ok(())
    }

    #[test]
    fn rejects_state_and_cross_function_comparisons_at_every_scope() -> ToolResult<()> {
        for mutation in 0..5 {
            let mut descriptor = fixture();
            match mutation {
                0 => descriptor.has_setup = true,
                1 => descriptor.has_teardown = true,
                2 => descriptor.groups[0].has_setup = true,
                3 => descriptor.groups[0].has_teardown = true,
                _ => descriptor.groups[0].compare_by_id = Some(true),
            }
            let (_temporary, directory) = evidence(&descriptor)?;
            assert!(
                plan(
                    &directory,
                    &identities(&descriptor),
                    Path::new("/frozen/analysis")
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn rejects_missing_extra_duplicate_empty_and_mixed_workloads() -> ToolResult<()> {
        let original = fixture();
        let expected = identities(&original);
        for mutation in 0..6 {
            let mut descriptor = original.clone();
            let benches = &mut descriptor.groups[1].library_benchmarks[1].benches;
            match mutation {
                0 => {
                    benches.pop();
                }
                1 => benches.push(bench("first", Some("extra"))),
                2 => benches.push(benches[0].clone()),
                3 => benches[0].function_name.clear(),
                4 => benches[0].id = Some(String::new()),
                _ => benches[0].function_name = "mixed".into(),
            }
            let (_temporary, directory) = evidence(&descriptor)?;
            assert!(plan(&directory, &expected, Path::new("/frozen/analysis")).is_err());
        }
        let (_temporary, directory) = evidence(&original)?;
        assert!(plan(&directory, &BTreeSet::new(), Path::new("/frozen/analysis")).is_err());
        assert!(plan(&directory, &expected, Path::new("/another/executable")).is_err());
        Ok(())
    }

    #[test]
    fn capture_requires_fresh_evidence_and_exact_original_framing() -> ToolResult<()> {
        let descriptor = bincode::serialize(&fixture())?;
        let args = arguments(descriptor.len());
        let temporary = tempfile::tempdir()?;
        let directory = temporary.path().join("capture");
        capture(&directory, &args, &mut Cursor::new(&descriptor))?;
        assert!(capture(&directory, &args, &mut Cursor::new(&descriptor)).is_err());
        for mutation in 0..7 {
            let mut invalid = args.clone();
            match mutation {
                0 => {
                    invalid.pop();
                }
                1 => invalid.push("extra".into()),
                2 => invalid[0] = "0.16.2".into(),
                3 => invalid[1] = "--bin-bench".into(),
                4 => invalid[7] = format!("0{}", descriptor.len()),
                5 => invalid[7] = (MAX_BYTES + 1).to_string(),
                _ => invalid[7] = (descriptor.len() - 1).to_string(),
            }
            let fresh = temporary.path().join(format!("invalid-{mutation}"));
            assert!(capture(&fresh, &invalid, &mut Cursor::new(&descriptor)).is_err());
            assert!(!fresh.exists());
        }
        let mut trailing = descriptor.clone();
        trailing.push(0);
        assert!(
            capture(
                &temporary.path().join("trailing"),
                &arguments(trailing.len()),
                &mut Cursor::new(trailing)
            )
            .is_err()
        );
        assert!(decode(&descriptor[..descriptor.len() - 1]).is_err());
        assert!(
            capture(
                &temporary.path().join("short"),
                &args,
                &mut Cursor::new(&descriptor[..descriptor.len() - 1])
            )
            .is_err()
        );
        assert!(
            capture(
                &temporary.path().join("oversized"),
                &args,
                &mut std::io::repeat(0)
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn bounds_input_and_revalidates_persisted_evidence() -> ToolResult<()> {
        assert_eq!(
            bounded_read(&mut std::io::repeat(0).take(MAX_BYTES as u64))?.len(),
            MAX_BYTES
        );
        assert!(bounded_read(&mut std::io::repeat(0)).is_err());
        let mut boundary = fixture();
        let unpadded = usize::try_from(bincode::serialized_size(&boundary)?)?;
        boundary.command_line_args[0].push_str(&"x".repeat(MAX_BYTES - unpadded));
        let maximum = bincode::serialize(&boundary)?;
        assert_eq!(maximum.len(), MAX_BYTES);
        let boundary_directory = tempfile::tempdir()?;
        let boundary_capture = boundary_directory.path().join("capture");
        capture(
            &boundary_capture,
            &arguments(MAX_BYTES),
            &mut Cursor::new(maximum),
        )?;
        assert_eq!(
            fs::metadata(boundary_capture.join("descriptor.bin"))?.len(),
            MAX_BYTES as u64
        );
        let original = fixture();
        let expected = identities(&original);
        let (_temporary, directory) = evidence(&original)?;
        let bytes = fs::read(directory.join("descriptor.bin"))?;
        let mut trailing = bytes.clone();
        trailing.push(0);
        fs::write(directory.join("descriptor.bin"), &trailing)?;
        fs::write(
            directory.join("argv.json"),
            serde_json::to_vec(&arguments(trailing.len()))?,
        )?;
        assert!(plan(&directory, &expected, Path::new("/frozen/analysis")).is_err());
        fs::write(directory.join("descriptor.bin"), &bytes)?;
        fs::write(
            directory.join("argv.json"),
            serde_json::to_vec(&arguments(bytes.len() + 1))?,
        )?;
        assert!(plan(&directory, &expected, Path::new("/frozen/analysis")).is_err());
        fs::write(directory.join("argv.json"), b"[] trailing")?;
        assert!(plan(&directory, &expected, Path::new("/frozen/analysis")).is_err());
        Ok(())
    }
}
