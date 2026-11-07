use super::export::{ExportError, MAX_REPORT_BYTES, create_private_report};
use crate::server::{lsp::Backend, state::blocking_result};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use tower_lsp::{
    jsonrpc::{Error, Result},
    lsp_types::{ExecuteCommandParams, MessageType},
};
use url::Url;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportArguments {
    uri: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WrittenReport {
    uri: String,
    bytes_written: u64,
    schema_version: u32,
}

struct BoundedReport {
    bytes: Vec<u8>,
    rejected: bool,
}

impl Write for BoundedReport {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_REPORT_BYTES.saturating_sub(self.bytes.len()) {
            self.rejected = true;
            return Err(io::Error::other("diagnostic snapshot exceeds its size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_report(destination: Option<String>, report: &impl Serialize) -> std::result::Result<WrittenReport, ExportError> {
    let mut payload = BoundedReport { bytes: Vec::new(), rejected: false };
    if serde_json::to_writer_pretty(&mut payload, report).is_err() {
        return Err(if payload.rejected {
            ExportError::PayloadTooLarge
        } else {
            ExportError::Io(io::ErrorKind::InvalidData)
        });
    }
    let directory = if destination.is_none() {
        Some(tempfile::Builder::new().prefix("bend2-lsp-diagnostics-").tempdir()
            .map_err(|error| ExportError::Io(error.kind()))?)
    } else {
        None
    };
    let uri = match destination {
        Some(uri) => uri,
        None => {
            let root = directory.as_ref().ok_or(ExportError::MissingParent)?;
            Url::from_file_path(root.path().join("snapshot.json"))
                .map_err(|()| ExportError::InvalidUri)?
                .to_string()
        }
    };
    let outcome = create_private_report(uri.as_str(), &payload.bytes)?;
    if let Some(directory) = directory {
        let _ = directory.keep();
    }
    Ok(WrittenReport { uri, bytes_written: outcome.bytes_written, schema_version: 1 })
}

impl Backend {
    #[tracing::instrument(name = "diagnostics.export", skip_all)]
    pub(in crate::server) async fn handle_diagnostic_export(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        let mut arguments = params.arguments;
        let destination = match arguments.pop() {
            None => None,
            Some(argument) if arguments.is_empty() => Some(
                serde_json::from_value::<ExportArguments>(argument)
                    .map_err(|_| Error::invalid_params("Expected one object containing a file URI."))?
                    .uri,
            ),
            Some(_) => return Err(Error::invalid_params("Expected no arguments or one destination object.")),
        };
        let report = self.workspace.observability.collect_report().await
            .map_err(|_| Error::internal_error())?;
        let written = blocking_result(tokio::task::spawn_blocking(move || write_report(destination, &report)).await)
            .map_err(|error| Error::invalid_params(error.to_string()))?;
        self.client.show_message(
            MessageType::INFO,
            format!("Diagnostic snapshot saved to {}. The local report excludes source text and source paths.", written.uri),
        ).await;
        serde_json::to_value(written).map(Some).map_err(|_| Error::internal_error())
    }
}
