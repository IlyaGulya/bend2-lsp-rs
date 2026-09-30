use bend2_lsp::analysis::{DocumentSnapshot, Revision, folding_ranges};

fn main() {
    let mut arguments = std::env::args().skip(1);
    let Some(lines) = arguments
        .next()
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return;
    };
    let fold = arguments.next().is_some_and(|mode| mode == "fold");
    let mut source = String::with_capacity(lines * 7);
    for line in 0..lines {
        if line > 0 {
            source.push('\n');
        }
        source.push_str(if line % 2 == 0 { "block" } else { "  body" });
    }
    let snapshot = DocumentSnapshot::new(Revision(0), source);
    std::hint::black_box(&snapshot);
    if fold {
        std::hint::black_box(folding_ranges(&snapshot));
    }
}
