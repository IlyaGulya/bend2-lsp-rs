pub mod analysis;
mod server;
pub mod workspace;

/// Serve the LSP protocol on standard input and output.
///
/// # Errors
///
/// Returns an error after draining owned work if a server invariant or
/// supervised background task fails.
pub async fn run() -> std::io::Result<()> {
    server::run().await
}
