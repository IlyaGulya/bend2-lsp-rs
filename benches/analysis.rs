use std::hint::black_box;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};

/// Analyzer module made public only within this benchmark crate so all production APIs remain reachable.
#[path = "../src/analysis.rs"]
pub mod analysis;

const SOURCE: &str = include_str!("fixtures/analyzer_input.bend");

#[library_benchmark]
fn semantic_tokens() -> Vec<tower_lsp::lsp_types::SemanticToken> {
    black_box(analysis::semantic_tokens(black_box(SOURCE)))
}

#[library_benchmark]
fn completion_items() -> Vec<tower_lsp::lsp_types::CompletionItem> {
    black_box(analysis::completion_items(
        black_box(SOURCE),
        black_box("transform_"),
    ))
}

#[library_benchmark]
fn identifier_ranges() -> Vec<tower_lsp::lsp_types::Range> {
    black_box(analysis::identifier_ranges(
        black_box(SOURCE),
        black_box("transform_31"),
    ))
}

library_benchmark_group!(
    name = analysis_hot_paths;
    benchmarks = semantic_tokens, completion_items, identifier_ranges
);

main!(library_benchmark_groups = analysis_hot_paths);
