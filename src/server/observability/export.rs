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
    fmt, io,
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

#[cfg(unix)]
#[path = "export_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "export_windows.rs"]
mod platform;

/// Create a new owner-private directory for a no-argument diagnostic export.
/// A file is only created in it after its access policy is established.
pub(crate) fn create_private_directory() -> Result<PathBuf, ExportError> {
    platform::private_directory(&std::env::temp_dir(), "bend2-lsp-diagnostics-")
}

/// Creates a brand-new owner-private report, publishing a fully written file
/// without replacing an existing entry. No report bytes enter a pathname that
/// was exposed with an inherited foreign read grant.
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
    platform::publish_report(&destination, payload)?;
    let bytes_written = u64::try_from(payload.len()).map_err(|_| ExportError::PayloadTooLarge)?;
    Ok(ReportOutcome { bytes_written })
}

fn report_path(uri: &str) -> Result<PathBuf, ExportError> {
    // Url normalizes file:relative to an absolute-looking path. Any later
    // violation must not erase the missing-double-slash observation.
    let missing_double_slash = std::cell::Cell::new(false);
    let malformed = std::cell::Cell::new(false);
    let url = url::Url::options()
        .syntax_violation_callback(Some(&|kind| {
            if kind == url::SyntaxViolation::ExpectedFileDoubleSlash {
                missing_double_slash.set(true);
            } else if matches!(
                kind,
                url::SyntaxViolation::PercentDecode
                    | url::SyntaxViolation::C0SpaceIgnored
                    | url::SyntaxViolation::TabOrNewlineIgnored
                    | url::SyntaxViolation::NullInFragment
                    | url::SyntaxViolation::Backslash
                    | url::SyntaxViolation::EmbeddedCredentials
                    | url::SyntaxViolation::FileWithHostAndWindowsDrive
            ) {
                malformed.set(true);
            }
        }))
        .parse(uri)
        .map_err(|_| ExportError::InvalidUri)?;
    if missing_double_slash.get() {
        return Err(ExportError::RelativePath);
    }
    if malformed.get() {
        return Err(ExportError::InvalidUri);
    }
    if url.scheme() != "file" {
        return Err(ExportError::NonFileUri);
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ExportError::InvalidUri);
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

fn stage_name(prefix: &str) -> String {
    use std::{
        hash::BuildHasher,
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT: AtomicU64 = AtomicU64::new(0);
    // Exclusivity, not secrecy of this name, is the security boundary. The
    // randomized name keeps collisions rare; create_new/mkdirat decide them.
    let nonce = std::collections::hash_map::RandomState::new()
        .hash_one((std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    format!("{prefix}{nonce:016x}")
}

fn ensure_absent(destination: &Path) -> Result<(), ExportError> {
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(ExportError::SymlinkDestination),
        Ok(_) => Err(ExportError::ExistingDestination),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ExportError::Io(error.kind())),
    }
}

// Only the native Windows regression fixture invokes icacls to introduce an
// explicit foreign ACE. Production creation never launches a helper.
#[cfg(all(windows, test))]
fn icacls_path() -> Result<PathBuf, ExportError> {
    let directory =
        winsafe::GetSystemDirectory().map_err(|_| ExportError::PermissionRestriction)?;
    Ok(PathBuf::from(directory).join("icacls.exe"))
}

#[cfg(windows)]
pub(super) fn current_user_sid() -> Result<String, ExportError> {
    let token = winsafe::HPROCESS::GetCurrentProcess()
        .OpenProcessToken(winsafe::co::TOKEN::QUERY)
        .map_err(|_| ExportError::PermissionRestriction)?;
    let information = token
        .GetTokenInformation(winsafe::co::TOKEN_INFORMATION_CLASS::User)
        .map_err(|_| ExportError::PermissionRestriction)?;
    let winsafe::TokenInfo::User(user) = information else {
        return Err(ExportError::PermissionRestriction);
    };
    let sid = user.User.Sid().ok_or(ExportError::PermissionRestriction)?;
    winsafe::ConvertSidToStringSid(sid).map_err(|_| ExportError::PermissionRestriction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_uri(path: &Path) -> String {
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
    fn rejects_single_slash_even_with_another_uri_syntax_violation() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("report%GG.json");
        let uri = format!("file:{}", destination.display());
        assert_eq!(
            create_private_report(&uri, b"{}"),
            Err(ExportError::RelativePath)
        );
        assert!(!destination.exists());
    }

    #[test]
    fn rejects_malformed_percent_escape_even_with_an_absolute_file_uri() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("report%GG.json");
        let uri = format!(
            "{}/report%GG.json",
            file_uri(directory.path()).trim_end_matches('/')
        );
        assert_eq!(
            create_private_report(&uri, b"{}"),
            Err(ExportError::InvalidUri)
        );
        assert!(!destination.exists());
    }

    #[test]
    fn rejects_query_and_fragment_instead_of_silently_stripping_them() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("report.json");
        for suffix in ["?download=1", "#section", "?download=1#section"] {
            let uri = format!("{}{}", file_uri(&destination), suffix);
            assert_eq!(
                create_private_report(&uri, b"{}"),
                Err(ExportError::InvalidUri)
            );
        }
        assert!(!destination.exists());
    }

    #[test]
    fn accepts_encoded_punctuation_spaces_and_unicode_in_local_file_names() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let destination = directory.path().join("diagnostic ? # résumé.json");
        let uri = file_uri(&destination);
        assert!(uri.contains("%3F") && uri.contains("%23"));
        assert_eq!(
            create_private_report(&uri, b"{}"),
            Ok(ReportOutcome { bytes_written: 2 })
        );
        assert_eq!(std::fs::read(&destination).expect("read report"), b"{}");
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
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("report.json");
        let uri = file_uri(&destination);
        let payload = br#"{"schemaVersion":1}"#;

        let outcome = create_private_report(&uri, payload)?;
        assert_eq!(outcome.bytes_written, payload.len() as u64);
        assert_eq!(std::fs::read(&destination)?, payload);
        let entries = std::fs::read_dir(directory.path())?.collect::<Result<Vec<_>, _>>()?;
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
    fn report_mode_is_owner_only() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("report.json");
        create_private_report(&file_uri(&destination), b"{}")?;
        let mode = std::fs::metadata(&destination)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn generated_private_directory_refuses_inheritable_foreign_parent_acl()
    -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        let grant = std::process::Command::new("/bin/chmod")
            .args([
                "+a",
                "everyone allow read,readattr,readextattr,file_inherit,directory_inherit",
            ])
            .arg(root.path())
            .status()?;
        assert!(grant.success(), "cannot create inheritable ACL fixture");
        assert_eq!(
            platform::private_directory(root.path(), "generated-private-"),
            Err(ExportError::PermissionRestriction)
        );
        assert_eq!(
            std::fs::read_dir(root.path())?.count(),
            0,
            "private directory was exposed before its ACL was restricted"
        );
        Ok(())
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inherited_read_acl_is_absent_from_published_report() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let acl = "everyone allow read,readattr,readextattr,file_inherit,directory_inherit";
        let chmod = std::process::Command::new("/bin/chmod")
            .args(["+a", acl])
            .arg(directory.path())
            .status()?;
        assert!(chmod.success(), "failed to add inheritable read ACL");
        let inherited_probe = directory.path().join("inherited-probe");
        std::fs::write(&inherited_probe, b"")?;
        let probe = std::process::Command::new("/bin/ls")
            .env("LC_ALL", "C")
            .arg("-le")
            .arg(&inherited_probe)
            .output()?;
        assert!(
            probe.status.success(),
            "cannot inspect inherited ACL fixture"
        );
        let inherited_acl = String::from_utf8(probe.stdout)?;
        let grants_everyone = |acl: &str| {
            acl.lines()
                .any(|line| line.contains("group:everyone") && line.contains("allow"))
        };
        assert!(
            grants_everyone(&inherited_acl),
            "inheritable ACL fixture failed to grant foreign read: {inherited_acl}"
        );
        std::fs::remove_file(&inherited_probe)?;
        let destination = directory.path().join("report.json");
        create_private_report(&file_uri(&destination), b"private")?;
        let output = std::process::Command::new("/bin/ls")
            .env("LC_ALL", "C")
            .arg("-le")
            .arg(&destination)
            .output()?;
        assert!(output.status.success(), "cannot inspect report ACL");
        let acl = String::from_utf8(output.stdout)?;
        assert!(!grants_everyone(&acl), "foreign read ACL survived: {acl}");
        Ok(())
    }

    #[cfg(windows)]
    fn file_acl(
        path: &Path,
    ) -> Result<
        windows_permissions::LocalBox<windows_permissions::SecurityDescriptor>,
        Box<dyn Error>,
    > {
        use windows_permissions::{
            constants::{SeObjectType, SecurityInformation},
            wrappers::GetNamedSecurityInfo,
        };
        Ok(GetNamedSecurityInfo(
            path,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        )?)
    }

    #[cfg(windows)]
    fn assert_protected_owner_acl(path: &Path) -> Result<(), Box<dyn Error>> {
        use windows_permissions::constants::{AccessRights, AceFlags, AceType};
        let descriptor = file_acl(path)?;
        let dacl = descriptor.dacl().expect("DACL must not be absent or NULL");
        assert_eq!(dacl.len(), 1, "exactly one owner ACE is required");
        let ace = dacl.get_ace(0).expect("owner ACE");
        assert_eq!(
            ace.sid().expect("owner SID").to_string(),
            current_user_sid()?
        );
        assert_eq!(
            ace.ace_type(),
            AceType::ACCESS_ALLOWED_ACE_TYPE,
            "owner ACE must grant rather than deny access"
        );
        assert!(
            ace.mask().contains(AccessRights::FileAllAccess),
            "owner lacks full control"
        );
        assert!(
            !ace.flags().contains(AceFlags::Inherited),
            "inherited ACE survives"
        );
        let sddl =
            windows_permissions::wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
                &descriptor,
                windows_permissions::constants::SecurityInformation::Dacl,
            )?;
        assert!(
            sddl.to_string_lossy().contains("D:P"),
            "DACL is not protected: {sddl:?}"
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn refuses_explicit_foreign_read_ace_in_a_report_security_descriptor()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("foreign-grant.json");
        std::fs::write(&destination, b"")?;
        let grant = std::process::Command::new(icacls_path()?)
            .arg(&destination)
            .args(["/grant", "*S-1-1-0:R"])
            .status()?;
        assert!(grant.success(), "cannot add explicit foreign read ACE");
        assert!(
            file_acl(&destination)?.dacl().expect("DACL").len() > 1,
            "fixture must include explicit foreign ACE"
        );
        assert_eq!(
            platform::verify_owner_acl(&destination),
            Err(ExportError::PermissionRestriction)
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn explicit_foreign_parent_read_grant_does_not_open_published_report()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let grant = std::process::Command::new(icacls_path()?)
            .arg(directory.path())
            .args(["/grant", "*S-1-1-0:R"])
            .status()?;
        assert!(grant.success(), "cannot add explicit foreign read ACE");
        let before = file_acl(directory.path())?;
        assert!(
            before.dacl().expect("parent DACL").len() > 1,
            "explicit foreign read fixture was not applied"
        );
        let destination = directory.path().join("report.json");
        create_private_report(&file_uri(&destination), b"private")?;
        assert_protected_owner_acl(&destination)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn inherited_read_grant_is_absent_from_new_report() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let grant = std::process::Command::new(icacls_path()?)
            .arg(directory.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .status()?;
        assert!(grant.success(), "cannot add inheritable foreign ACE");
        let probe = directory.path().join("inherited-probe");
        std::fs::write(&probe, b"")?;
        let inherited = file_acl(&probe)?;
        assert!(
            inherited.dacl().expect("probe DACL").len() > 1,
            "inheritable foreign ACE did not propagate to a child"
        );
        std::fs::remove_file(&probe)?;
        let destination = directory.path().join("report.json");
        create_private_report(&file_uri(&destination), b"private")?;
        assert_protected_owner_acl(&destination)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn report_acl_is_owner_only_without_inherited_entries() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("report.json");
        create_private_report(&file_uri(&destination), b"{}")?;

        assert_protected_owner_acl(&destination)?;
        Ok(())
    }
}
