//! Owner-private creation of bounded local diagnostic report files.
//!
//! The destination is always a brand-new regular file chosen by the user; an
//! existing file, directory or symlink is never replaced. Serialization stays
//! outside this module so reports cannot accidentally include request bodies,
//! source text or server-chosen paths from here. All functions block on
//! ordinary file-system operations and must run under `spawn_blocking`.

use serde::Serialize;
use std::{
    error::Error,
    fmt,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Serialized report size cap. History and aggregates are small; anything
/// larger is rejected before a temporary file or destination is created.
pub(crate) const MAX_REPORT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportError {
    InvalidUri,
    NonFileUri,
    RemoteHost,
    RelativePath,
    ExistingDestination,
    SymlinkDestination,
    MissingParent,
    ParentNotDirectory,
    EmptyPayload,
    PayloadTooLarge,
    PermissionRestriction,
    Io(io::ErrorKind),
}

impl fmt::Display for ExportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUri => {
                formatter.write_str("diagnostic report URI is not a valid absolute file URI")
            }
            Self::NonFileUri => {
                formatter.write_str("diagnostic report URI must use the file scheme")
            }
            Self::RemoteHost => {
                formatter.write_str("diagnostic report URI must be a local file URI")
            }
            Self::RelativePath => {
                formatter.write_str("diagnostic report URI must resolve to an absolute local path")
            }
            Self::ExistingDestination => {
                formatter.write_str("diagnostic report destination already exists")
            }
            Self::SymlinkDestination => {
                formatter.write_str("diagnostic report destination must not be a symlink")
            }
            Self::MissingParent => {
                formatter.write_str("diagnostic report destination directory does not exist")
            }
            Self::ParentNotDirectory => {
                formatter.write_str("diagnostic report destination parent is not a directory")
            }
            Self::EmptyPayload => formatter.write_str("diagnostic report payload is empty"),
            Self::PayloadTooLarge => {
                formatter.write_str("diagnostic report payload exceeds the export limit")
            }
            Self::PermissionRestriction => {
                formatter.write_str("diagnostic report could not be made owner-private")
            }
            Self::Io(kind) => write!(formatter, "diagnostic report I/O failed ({kind:?})"),
        }
    }
}

impl Error for ExportError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReportOutcome {
    pub(crate) bytes_written: u64,
}

/// Atomically creates a new owner-private report at an absolute local file
/// URI. Never overwrites, never follows a destination symlink, and removes its
/// temporary file if any step fails. No diagnostic content beyond `payload`
/// is written, and nothing is sent to stdout.
pub(crate) fn create_private_report(
    uri: &str,
    payload: &[u8],
) -> Result<ReportOutcome, ExportError> {
    let destination = report_path(uri)?;
    if payload.is_empty() {
        return Err(ExportError::EmptyPayload);
    }
    if payload.len() > MAX_REPORT_BYTES {
        return Err(ExportError::PayloadTooLarge);
    }
    let parent = destination
        .parent()
        .ok_or(ExportError::ParentNotDirectory)?;
    let parent_metadata = std::fs::metadata(parent).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => ExportError::MissingParent,
        kind => ExportError::Io(kind),
    })?;
    if !parent_metadata.is_dir() {
        return Err(ExportError::ParentNotDirectory);
    }
    ensure_absent(&destination)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".bend2-diagnostic-report-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|error| ExportError::Io(error.kind()))?;
    // Restrict before writing so report bytes are never visible under an
    // inherited permissive ACL on platforms where the helper applies one.
    restrict_to_owner(temporary.path(), temporary.as_file())?;
    temporary
        .write_all(payload)
        .map_err(|error| ExportError::Io(error.kind()))?;
    temporary
        .flush()
        .map_err(|error| ExportError::Io(error.kind()))?;

    match temporary.persist_noclobber(&destination) {
        Ok(_) => {
            let bytes_written =
                u64::try_from(payload.len()).map_err(|_| ExportError::PayloadTooLarge)?;
            Ok(ReportOutcome { bytes_written })
        }
        Err(error) => Err(match error.error.kind() {
            // A destination that appeared after the pre-check still cannot be
            // replaced; `persist_noclobber` guarantees a no-clobber commit.
            io::ErrorKind::AlreadyExists => ExportError::ExistingDestination,
            kind => ExportError::Io(kind),
        }),
    }
}

fn report_path(uri: &str) -> Result<PathBuf, ExportError> {
    // `Url` normalizes `file:relative` to an absolute-looking path, so the
    // syntax violation is the only reliable way to reject a URI that never
    // named an authority (`file:///`).
    let violation = std::cell::Cell::new(None);
    let url = url::Url::options()
        .syntax_violation_callback(Some(&|kind| violation.set(Some(kind))))
        .parse(uri)
        .map_err(|_| ExportError::InvalidUri)?;
    if matches!(
        violation.get(),
        Some(url::SyntaxViolation::ExpectedFileDoubleSlash)
    ) {
        return Err(ExportError::RelativePath);
    }
    if url.scheme() != "file" {
        return Err(ExportError::NonFileUri);
    }
    match url.host_str() {
        None | Some("" | "localhost") => {}
        Some(_) => return Err(ExportError::RemoteHost),
    }
    let path = url.to_file_path().map_err(|()| ExportError::RelativePath)?;
    if !path.is_absolute() {
        return Err(ExportError::RelativePath);
    }
    Ok(path)
}

fn ensure_absent(destination: &Path) -> Result<(), ExportError> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ExportError::SymlinkDestination),
        Ok(_) => Err(ExportError::ExistingDestination),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ExportError::Io(error.kind())),
    }
}

#[cfg(unix)]
fn restrict_to_owner(_path: &Path, file: &std::fs::File) -> Result<(), ExportError> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| ExportError::Io(error.kind()))?;
    let mode = file
        .metadata()
        .map_err(|error| ExportError::Io(error.kind()))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(ExportError::PermissionRestriction);
    }
    Ok(())
}

#[cfg(windows)]
fn restrict_to_owner(path: &Path, _file: &std::fs::File) -> Result<(), ExportError> {
    apply_owner_only_acl(path)
}

#[cfg(not(any(unix, windows)))]
fn restrict_to_owner(_path: &Path, _file: &std::fs::File) -> Result<(), ExportError> {
    Err(ExportError::PermissionRestriction)
}

/// Applies an explicit DACL containing only the current user; inherited ACEs
/// are removed rather than assumed to be restrictive. `icacls` is the bounded
/// system helper used because the pinned safe API exposes tokens and SIDs but
/// no DACL writer. The helper runs in a kill-on-close job object and has a
/// short deadline; its output is discarded.
#[cfg(windows)]
fn apply_owner_only_acl(path: &Path) -> Result<(), ExportError> {
    use process_wrap::std::{ChildWrapper, CommandWrap, JobObject};
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };

    const HELPER_TIMEOUT: Duration = Duration::from_secs(10);

    let sid = current_user_sid()?;
    let grant = format!("*{sid}:F");
    let mut command = CommandWrap::with_new(icacls_path()?, |command| {
        command
            .arg(path)
            .args(["/inheritance:r", "/grant:r", grant.as_str()])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    });
    command.wrap(JobObject);
    let mut child: Box<dyn ChildWrapper> = command
        .spawn()
        .map_err(|error| ExportError::Io(error.kind()))?;

    let deadline = Instant::now() + HELPER_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(ExportError::PermissionRestriction)
                };
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                return Err(ExportError::Io(error.kind()));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return Err(ExportError::PermissionRestriction);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn icacls_path() -> Result<PathBuf, ExportError> {
    let root = std::env::var_os("SystemRoot").ok_or(ExportError::PermissionRestriction)?;
    let path = PathBuf::from(root).join("System32").join("icacls.exe");
    if !path.is_file() {
        return Err(ExportError::PermissionRestriction);
    }
    Ok(path)
}

#[cfg(windows)]
fn current_user_sid() -> Result<String, ExportError> {
    let token = winsafe::HPROCESS::GetCurrentProcess()
        .OpenProcessToken(winsafe::co::TOKEN::QUERY)
        .map_err(|_| ExportError::PermissionRestriction)?;
    let information = token
        .GetTokenInformation(winsafe::co::TOKEN_INFORMATION_CLASS::User)
        .map_err(|_| ExportError::PermissionRestriction)?;
    let winsafe::TokenInfo::User(user) = information else {
        return Err(ExportError::PermissionRestriction);
    };
    let sid = user
        .User
        .Sid()
        .ok_or(ExportError::PermissionRestriction)?;
    winsafe::ConvertSidToStringSid(sid).map_err(|_| ExportError::PermissionRestriction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_uri(path: &std::path::Path) -> String {
        url::Url::from_file_path(path)
            .expect("absolute temporary path has a file URI")
            .to_string()
    }

    #[test]
    fn rejects_non_file_relative_and_remote_uris() {
        for uri in [
            "relative/report.json",
            "http://example.com/report.json",
            "file:relative.json",
            "file://remote-host/share/report.json",
        ] {
            assert!(
                matches!(
                    create_private_report(uri, b"{}"),
                    Err(ExportError::InvalidUri
                        | ExportError::NonFileUri
                        | ExportError::RemoteHost
                        | ExportError::RelativePath)
                ),
                "URI should be rejected: {uri}"
            );
        }

        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("report.json");
        let single_slash = format!("file:{}", destination.display());
        assert_eq!(
            create_private_report(&single_slash, b"{}"),
            Err(ExportError::RelativePath)
        );
        assert!(!destination.exists());
    }

    #[test]
    fn rejects_empty_and_oversized_payloads_without_creating_a_destination() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("report.json");
        let uri = file_uri(&destination);

        assert_eq!(
            create_private_report(&uri, b""),
            Err(ExportError::EmptyPayload)
        );
        let oversized = vec![b' '; MAX_REPORT_BYTES + 1];
        assert_eq!(
            create_private_report(&uri, &oversized),
            Err(ExportError::PayloadTooLarge)
        );
        assert!(!destination.exists());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("read temporary directory")
                .count(),
            0
        );
    }

    #[test]
    fn rejects_missing_parent_and_non_directory_parent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let missing = directory.path().join("missing").join("report.json");
        assert_eq!(
            create_private_report(&file_uri(&missing), b"{}"),
            Err(ExportError::MissingParent)
        );

        let not_directory = directory.path().join("not-a-directory");
        std::fs::write(&not_directory, b"occupied").expect("write parent file");
        let nested = not_directory.join("report.json");
        assert_eq!(
            create_private_report(&file_uri(&nested), b"{}"),
            Err(ExportError::ParentNotDirectory)
        );
    }

    #[test]
    fn creates_exact_report_without_leftover_temporary_and_never_overwrites()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("report.json");
        let uri = file_uri(&destination);
        let payload = br#"{"schemaVersion":1}"#;

        let outcome = create_private_report(&uri, payload)?;
        assert_eq!(outcome.bytes_written, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        let entries = std::fs::read_dir(directory.path())?
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(entries.len(), 1, "temporary file was not cleaned up");

        assert_eq!(
            create_private_report(&uri, b"replacement"),
            Err(ExportError::ExistingDestination)
        );
        assert_eq!(std::fs::read(&destination)?, payload);
        Ok(())
    }

    #[test]
    fn rejects_existing_regular_file_and_preserves_contents() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("report.json");
        std::fs::write(&destination, b"keep me").expect("write existing report");

        assert_eq!(
            create_private_report(&file_uri(&destination), b"replacement"),
            Err(ExportError::ExistingDestination)
        );
        assert_eq!(
            std::fs::read(&destination).expect("read existing report"),
            b"keep me"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_destination_and_preserves_link_target() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("target.json");
        std::fs::write(&target, b"original").expect("write symlink target");
        let destination = directory.path().join("report.json");
        std::os::unix::fs::symlink(&target, &destination).expect("create destination symlink");

        assert_eq!(
            create_private_report(&file_uri(&destination), b"replacement"),
            Err(ExportError::SymlinkDestination)
        );
        assert_eq!(
            std::fs::read(&target).expect("read symlink target"),
            b"original"
        );
        assert!(
            std::fs::symlink_metadata(&destination)
                .expect("destination still present")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn report_mode_is_owner_only() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("report.json");
        create_private_report(&file_uri(&destination), b"{}")?;
        let mode = std::fs::metadata(&destination)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn report_acl_is_owner_only_without_inherited_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("report.json");
        create_private_report(&file_uri(&destination), b"{}")?;

        let output = std::process::Command::new(icacls_path()?)
            .arg(&destination)
            .output()?;
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "icacls failed: {text}");
        assert!(!text.contains("(I)"), "inherited ACE remains: {text}");
        assert_eq!(
            text.matches(":(F)").count(),
            1,
            "expected exactly one full-control ACE: {text}"
        );
        Ok(())
    }
}
