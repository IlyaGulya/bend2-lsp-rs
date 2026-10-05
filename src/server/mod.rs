mod adapters;
mod capabilities;
mod compiler;
mod compiler_service;
mod diagnostics;
mod document_text;
mod features;
mod formatter;
mod lsp;
mod orchestration;
mod reference_locations;
mod requests;
mod revision;
mod state;
mod telemetry;
mod transport;
mod workspace_service;
#[cfg(test)]
mod workspace_tests;

pub(super) async fn run() -> std::io::Result<()> {
    transport::run().await
}
