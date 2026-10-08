#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() -> std::io::Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _profiler = std::env::var_os("BEND2_LSP_DHAT_FILE").map(|path| {
        let mut builder = dhat::Profiler::builder().trim_backtraces(Some(usize::MAX));
        if !path.is_empty() {
            builder = builder.file_name(path);
        }
        builder.build()
    });

    // This call owns the Tokio runtime, which is torn down before the profiler.
    run()
}

#[tokio::main]
async fn run() -> std::io::Result<()> {
    bend2_lsp::run().await
}
