use bend2_lsp::analysis::{DocumentSnapshot, Revision, folding_ranges};

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    #[cfg(feature = "dhat-heap")]
    let _profiler = std::env::var_os("BEND2_LSP_DHAT_FILE").map(|path| {
        let mut builder = dhat::Profiler::builder().trim_backtraces(Some(usize::MAX));
        if !path.is_empty() {
            builder = builder.file_name(path);
        }
        builder.build()
    });

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
