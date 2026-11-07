use super::{
    export::{ExportError, MAX_REPORT_BYTES, create_private_directory, create_private_report},
    work::WorkKind,
};
use crate::server::lsp::Backend;
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
        Some(create_private_directory()?)
    } else {
        None
    };
    let uri = match destination {
        Some(uri) => uri,
        None => {
            let root = directory.as_ref().ok_or(ExportError::MissingParent)?;
            match Url::from_file_path(root.join("snapshot.json")) {
                Ok(uri) => uri.to_string(),
                Err(()) => {
                    // Only an empty directory is removed. Never recursively
                    // follow an entry another writer may have substituted.
                    let _ = std::fs::remove_dir(root);
                    return Err(ExportError::InvalidUri);
                }
            }
        }
    };
    let outcome = create_private_report(uri.as_str(), &payload.bytes);
    if let (Some(directory), Err(_)) = (&directory, &outcome) {
        // Never traverse a pathname that a writable temporary parent could
        // have replaced; only an empty directory is eligible for removal.
        let _ = std::fs::remove_dir(directory);
    }
    let outcome = outcome?;
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
        let written = self.workspace.observability.blocking(
            WorkKind::SnapshotExport,
            move || write_report(destination, &report),
        ).await.ok_or_else(Error::internal_error)?
            .map_err(|error| Error::invalid_params(error.to_string()))?;
        self.client.show_message(
            MessageType::INFO,
            format!("Diagnostic snapshot saved to {}. The local report excludes source text and source paths.", written.uri),
        ).await;
        serde_json::to_value(written).map(Some).map_err(|_| Error::internal_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn no_argument_export_creates_owner_private_directory() {
        use std::os::unix::fs::PermissionsExt;
        let written = write_report(None, &serde_json::json!({"schemaVersion": 1}))
            .expect("no-argument export");
        let file = Url::parse(&written.uri).expect("result URI")
            .to_file_path().expect("local report path");
        let parent = file.parent().expect("report parent");
        let mode = std::fs::metadata(parent).expect("report directory")
            .permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "report directory must not be readable by others");
        assert!(file.is_file());
        std::fs::remove_file(&file).expect("remove test report");
        std::fs::remove_dir(parent).expect("remove test directory");
    }

    #[cfg(windows)]
    #[test]
    fn no_argument_export_does_not_grant_everyone_directory_read() {
        let written = write_report(None, &serde_json::json!({"schemaVersion": 1}))
            .expect("no-argument export");
        let file = Url::parse(&written.uri).expect("result URI")
            .to_file_path().expect("local report path");
        let parent = file.parent().expect("report parent");
        use windows_permissions::{constants::{SeObjectType, SecurityInformation}, wrappers::GetNamedSecurityInfo};
        let descriptor = GetNamedSecurityInfo(parent, SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl).expect("inspect directory DACL");
        let acl = descriptor.dacl().expect("non-NULL directory DACL");
        assert_eq!(acl.len(), 1, "directory must grant only its owner");
        let owner = acl.get_ace(0).and_then(windows_permissions::Ace::sid)
            .expect("directory owner SID");
        assert_eq!(owner.to_string(), super::super::export::current_user_sid()
            .expect("current user SID"));
        let sddl = windows_permissions::wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
            &descriptor, SecurityInformation::Dacl).expect("directory DACL SDDL");
        assert!(sddl.to_string_lossy().contains("D:P"), "directory DACL unprotected: {sddl:?}");
        std::fs::remove_file(&file).expect("remove test report");
        std::fs::remove_dir(parent).expect("remove test directory");
    }
}
