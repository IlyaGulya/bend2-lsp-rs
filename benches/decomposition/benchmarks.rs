use std::sync::LazyLock;

use iai_callgrind::{
    Callgrind, EntryPoint, LibraryBenchmarkConfig, library_benchmark, library_benchmark_group,
};
use tower_lsp::lsp_types::{Location, WorkspaceEdit};

use super::{
    consumers::{CallGroup, ReferenceView},
    fixtures::{self, ColdSources, Fixture, InitialSources},
    stages::{self, PreparedReferences},
};

fn config(entry: &str) -> LibraryBenchmarkConfig {
    let mut config = LibraryBenchmarkConfig::default();
    config.tool(Callgrind::default().entry_point(EntryPoint::Custom(format!(
        "semantic_decomposition::decomposition::stages::{entry}"
    ))));
    config
}
#[library_benchmark(config = config("initial_build"))]
#[bench::hundred_files(InitialSources::new())]
fn workspace_initial_build(sources: InitialSources) -> InitialSources {
    std::hint::black_box(stages::initial_build(std::hint::black_box(sources)))
}

#[library_benchmark(config = config("cold_workspace_build"))]
#[bench::sparse_100(ColdSources::new(100, 3))]
#[bench::sparse_1000(ColdSources::new(1_000, 3))]
#[bench::sparse_10000(ColdSources::new(10_000, 3))]
#[bench::matched_100(ColdSources::new(100, 99))]
#[bench::matched_1000(ColdSources::new(1_000, 999))]
#[bench::matched_10000(ColdSources::new(10_000, 9_999))]
fn cold_workspace_build(sources: ColdSources) -> Fixture {
    std::hint::black_box(stages::cold_workspace_build(std::hint::black_box(sources)))
}

#[library_benchmark(config = config("reference_lookup"))]
#[bench::sparse_100(LazyLock::force(&fixtures::SPARSE_100))]
#[bench::sparse_1000(LazyLock::force(&fixtures::SPARSE_1000))]
#[bench::sparse_10000(LazyLock::force(&fixtures::SPARSE_10000))]
#[bench::matched_100(LazyLock::force(&fixtures::MATCHED_100))]
#[bench::matched_1000(LazyLock::force(&fixtures::MATCHED_1000))]
#[bench::matched_10000(LazyLock::force(&fixtures::MATCHED_10000))]
fn workspace_reference_lookup_warm(fixture: &'static Fixture) -> ReferenceView {
    std::hint::black_box(stages::reference_lookup(std::hint::black_box(fixture)))
}

#[library_benchmark(config = config("reference_materialize"))]
#[bench::sparse_100(args = (LazyLock::force(&fixtures::SPARSE_100)), setup = stages::prepare_references)]
#[bench::sparse_1000(args = (LazyLock::force(&fixtures::SPARSE_1000)), setup = stages::prepare_references)]
#[bench::sparse_10000(args = (LazyLock::force(&fixtures::SPARSE_10000)), setup = stages::prepare_references)]
#[bench::matched_100(args = (LazyLock::force(&fixtures::MATCHED_100)), setup = stages::prepare_references)]
#[bench::matched_1000(args = (LazyLock::force(&fixtures::MATCHED_1000)), setup = stages::prepare_references)]
#[bench::matched_10000(args = (LazyLock::force(&fixtures::MATCHED_10000)), setup = stages::prepare_references)]
fn workspace_reference_materialize_warm(input: PreparedReferences) -> Vec<Location> {
    std::hint::black_box(stages::reference_materialize(std::hint::black_box(&input)))
}

#[library_benchmark(config = config("incoming"))]
#[bench::sparse_100(LazyLock::force(&fixtures::SPARSE_100))]
#[bench::sparse_1000(LazyLock::force(&fixtures::SPARSE_1000))]
#[bench::sparse_10000(LazyLock::force(&fixtures::SPARSE_10000))]
#[bench::matched_100(LazyLock::force(&fixtures::MATCHED_100))]
#[bench::matched_1000(LazyLock::force(&fixtures::MATCHED_1000))]
#[bench::matched_10000(LazyLock::force(&fixtures::MATCHED_10000))]
fn workspace_incoming_calls_warm(fixture: &'static Fixture) -> Vec<CallGroup> {
    std::hint::black_box(stages::incoming(std::hint::black_box(fixture)))
}

#[library_benchmark(config = config("outgoing"))]
#[bench::sparse_100(LazyLock::force(&fixtures::SPARSE_100))]
#[bench::sparse_1000(LazyLock::force(&fixtures::SPARSE_1000))]
#[bench::sparse_10000(LazyLock::force(&fixtures::SPARSE_10000))]
#[bench::matched_100(LazyLock::force(&fixtures::MATCHED_100))]
#[bench::matched_1000(LazyLock::force(&fixtures::MATCHED_1000))]
#[bench::matched_10000(LazyLock::force(&fixtures::MATCHED_10000))]
fn workspace_outgoing_calls_warm(fixture: &'static Fixture) -> Vec<CallGroup> {
    std::hint::black_box(stages::outgoing(std::hint::black_box(fixture)))
}

#[library_benchmark(config = config("rename"))]
#[bench::sparse_100(LazyLock::force(&fixtures::SPARSE_100))]
#[bench::sparse_1000(LazyLock::force(&fixtures::SPARSE_1000))]
#[bench::sparse_10000(LazyLock::force(&fixtures::SPARSE_10000))]
#[bench::matched_100(LazyLock::force(&fixtures::MATCHED_100))]
#[bench::matched_1000(LazyLock::force(&fixtures::MATCHED_1000))]
#[bench::matched_10000(LazyLock::force(&fixtures::MATCHED_10000))]
fn workspace_rename_warm(fixture: &'static Fixture) -> WorkspaceEdit {
    std::hint::black_box(stages::rename(std::hint::black_box(fixture)))
}

#[cfg(feature = "decomp-identity")]
#[library_benchmark(config = config("cold_semantic_build"))]
#[bench::sparse_100(fixtures::Sources::new(100, 3))]
#[bench::sparse_1000(fixtures::Sources::new(1_000, 3))]
#[bench::sparse_10000(fixtures::Sources::new(10_000, 3))]
#[bench::matched_100(fixtures::Sources::new(100, 99))]
#[bench::matched_1000(fixtures::Sources::new(1_000, 999))]
#[bench::matched_10000(fixtures::Sources::new(10_000, 9_999))]
fn cold_workspace_semantic_build(sources: fixtures::Sources) -> Fixture {
    std::hint::black_box(stages::cold_semantic_build(std::hint::black_box(sources)))
}

#[cfg(not(feature = "decomp-identity"))]
library_benchmark_group!(
    name = decomposition_hot_paths;
    benchmarks = workspace_initial_build, cold_workspace_build, workspace_reference_lookup_warm, workspace_reference_materialize_warm, workspace_incoming_calls_warm, workspace_outgoing_calls_warm, workspace_rename_warm
);

#[cfg(feature = "decomp-identity")]
library_benchmark_group!(
    name = decomposition_hot_paths;
    benchmarks = workspace_initial_build, cold_workspace_build, workspace_reference_lookup_warm, workspace_reference_materialize_warm, workspace_incoming_calls_warm, workspace_outgoing_calls_warm, workspace_rename_warm, cold_workspace_semantic_build
);

iai_callgrind::main!(library_benchmark_groups = decomposition_hot_paths);

pub(super) fn run() {
    main();
}
