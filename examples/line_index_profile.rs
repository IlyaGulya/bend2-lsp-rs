use bend2_lsp::analysis::{DocumentSnapshot, LineIndex, Revision};

const SMALL_SOURCE: &str = include_str!("../benches/fixtures/analyzer_input.bend");
const MEDIUM_SOURCE: &str = include_str!("../benches/fixtures/analyzer_medium.bend");
const LARGE_SOURCE: &str = include_str!("../benches/fixtures/analyzer_large.bend");
const POSITION_ITERATIONS: usize = 100_000;

fn unicode_source() -> String {
    format!("{MEDIUM_SOURCE}\n#{}", "é".repeat(4096))
}

fn profile_position(source: &str) {
    let index = LineIndex::new(source);
    for _ in 0..POSITION_ITERATIONS {
        std::hint::black_box(index.position(source, source.len()));
    }
}

fn profile_snapshot(source: &str) {
    let snapshot = DocumentSnapshot::new(Revision(0), source.to_owned());
    std::hint::black_box(&snapshot);
}

fn main() {
    let Some(mode) = std::env::args().nth(1) else {
        eprintln!(
            "usage: line_index_profile <position-ascii|position-unicode|snapshot-small|snapshot-medium|snapshot-large|snapshot-medium-unicode>"
        );
        std::process::exit(2);
    };
    match mode.as_str() {
        "position-ascii" => profile_position(SMALL_SOURCE),
        "position-unicode" => profile_position(&unicode_source()),
        "snapshot-small" => profile_snapshot(SMALL_SOURCE),
        "snapshot-medium" => profile_snapshot(MEDIUM_SOURCE),
        "snapshot-large" => profile_snapshot(LARGE_SOURCE),
        "snapshot-medium-unicode" => profile_snapshot(&unicode_source()),
        _ => {
            eprintln!("unknown profile mode: {mode}");
            std::process::exit(2);
        }
    }
}
