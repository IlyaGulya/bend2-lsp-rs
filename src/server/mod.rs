mod adapters;
mod capabilities;
mod compiler;
mod features;
mod formatter;
mod lsp;
mod telemetry;
mod transport;
#[cfg(test)]
mod workspace_tests;

pub(super) async fn run() {
    transport::run().await;
}
