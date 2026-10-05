#[tokio::main]
async fn main() -> std::io::Result<()> {
    bend2_lsp::run().await
}
