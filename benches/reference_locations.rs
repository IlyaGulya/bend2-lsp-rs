#[path = "../src/server/reference_locations.rs"]
mod reference_locations;
mod support;

use iai_callgrind::{library_benchmark, library_benchmark_group, main};
use support::Must;

fn reference_location_case(count: usize) -> Vec<tower_lsp::lsp_types::Location> {
    use tower_lsp::lsp_types::{Location, Position, Range};

    (0..count)
        .map(|index| {
            let shuffled = index * 7919 % count;
            let uri = url::Url::parse(&format!(
                "file:///workspace/reference-order/module_{:05}.bend",
                shuffled / 4
            ))
            .must_be("reference ordering fixture URI");
            let line = u32::try_from(shuffled % 4).must_be("fixture line");
            Location {
                uri,
                range: Range::new(Position::new(line, 2), Position::new(line, 10)),
            }
        })
        .collect()
}

#[library_benchmark]
#[bench::small(reference_location_case(64))]
#[bench::medium(reference_location_case(1024))]
#[bench::large(reference_location_case(10000))]
fn reference_location_order_warm(
    mut locations: Vec<tower_lsp::lsp_types::Location>,
) -> Vec<tower_lsp::lsp_types::Location> {
    reference_locations::sort_and_deduplicate(std::hint::black_box(&mut locations));
    std::hint::black_box(locations)
}

library_benchmark_group!(
    name = reference_order_hot_paths;
    benchmarks = reference_location_order_warm
);

main!(library_benchmark_groups = reference_order_hot_paths);
