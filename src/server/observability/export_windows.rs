//! Windows creation with a protected owner-only DACL at the create syscall.
//!
//! While publishing, the private stage directory is held without delete
//! sharing so an entry-writable parent cannot replace the source pathname.

use super::{ExportError, current_user_sid, stage_name};
use std::{
    io,
    path::{Path, PathBuf},
};
use windows_permissions::{
    constants::{AccessRights, AceFlags, AceType, SeObjectType, SecurityInformation},
    wrappers::{ConvertSecurityDescriptorToStringSecurityDescriptor, GetNamedSecurityInfo},
};
use winsafe::{co, guard::CloseHandleGuard};

struct PrivateDirectory {
    path: PathBuf,
    handle: Option<CloseHandleGuard<winsafe::HFILE>>,
    cleanup: bool,
    created_report: bool,
}

impl PrivateDirectory {
    fn new(parent: &Path, prefix: &str) -> Result<Self, ExportError> {
        Self::new_with_created(parent, prefix, |_| {})
    }

    // Test seam for an entry-writable parent's substitution between creation
    // and acquisition of our directory handle.
    fn new_with_created(
        parent: &Path,
        prefix: &str,
        on_created: impl FnOnce(&Path),
    ) -> Result<Self, ExportError> {
        for _ in 0..16 {
            let path = parent.join(stage_name(prefix));
            let text = path.to_str().ok_or(ExportError::InvalidUri)?;
            let result = with_owner_acl(|attributes| {
                winsafe::CreateDirectory(text, Some(attributes)).map_err(win_error)
            });
            match result {
                Ok(()) => {}
                Err(ExportError::Io(io::ErrorKind::AlreadyExists)) => continue,
                Err(error) => return Err(error),
            }
            on_created(&path);
            let handle = match winsafe::HFILE::CreateFile(
                text,
                co::GENERIC::READ,
                Some(co::FILE_SHARE::READ | co::FILE_SHARE::WRITE),
                None,
                co::DISPOSITION::OPEN_EXISTING,
                co::FILE_ATTRIBUTE::NORMAL,
                Some(co::FILE_FLAG::BACKUP_SEMANTICS),
                None,
                None,
            ) {
                Ok((handle, _)) => handle,
                Err(error) => {
                    // No report exists yet. A single empty-directory removal
                    // cannot traverse an entry substituted by another writer.
                    let _ = std::fs::remove_dir(&path);
                    return Err(win_error(error));
                }
            };
            let directory = Self {
                path,
                handle: Some(handle),
                cleanup: true,
                created_report: false,
            };
            verify_owner_acl(&directory.path)?;
            return Ok(directory);
        }
        Err(ExportError::Io(io::ErrorKind::AlreadyExists))
    }

    fn keep(mut self) -> PathBuf {
        self.cleanup = false;
        self.path.clone()
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        if self.cleanup {
            // The directory is still held without FILE_SHARE_DELETE while we
            // remove its child: the pathname cannot be substituted, and its
            // DACL denies foreign writers. Do not recursively remove a path.
            if self.created_report {
                let _ = std::fs::remove_file(self.path.join("report"));
            }
            self.handle.take();
            // A renamed/replaced entry can only be removed if it is empty;
            // unrelated contents are never traversed or deleted.
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

pub(super) fn private_directory(parent: &Path, prefix: &str) -> Result<PathBuf, ExportError> {
    Ok(PrivateDirectory::new(parent, prefix)?.keep())
}

pub(super) fn publish_report(destination: &Path, payload: &[u8]) -> Result<(), ExportError> {
    publish_report_with_created(destination, payload, |_| {})
}

fn publish_report_with_created(
    destination: &Path,
    payload: &[u8],
    on_created: impl FnOnce(&Path),
) -> Result<(), ExportError> {
    let parent = destination
        .parent()
        .ok_or(ExportError::ParentNotDirectory)?;
    let mut stage =
        PrivateDirectory::new_with_created(parent, ".bend2-diagnostic-report-", on_created)?;
    let staged_path = stage.path.join("report");
    let staged_text = staged_path.to_str().ok_or(ExportError::InvalidUri)?;
    let (file, _) = with_owner_acl(|attributes| {
        winsafe::HFILE::CreateFile(
            staged_text,
            co::GENERIC::WRITE,
            Some(co::FILE_SHARE::READ | co::FILE_SHARE::WRITE),
            Some(attributes),
            co::DISPOSITION::CREATE_NEW,
            co::FILE_ATTRIBUTE::NORMAL,
            None,
            None,
            None,
        )
        .map_err(win_error)
    })?;
    stage.created_report = true;
    verify_owner_acl(&staged_path)?;
    let mut remaining = payload;
    while !remaining.is_empty() {
        let count = file.WriteFile(remaining).map_err(win_error)? as usize;
        if count == 0 {
            return Err(ExportError::Io(io::ErrorKind::WriteZero));
        }
        remaining = &remaining[count..];
    }
    // MoveFileW is an atomic same-volume rename which refuses to overwrite an
    // existing destination. Release only the source file handle: the held,
    // owner-private stage directory still prevents pathname substitution.
    drop(file);
    let destination_text = destination.to_str().ok_or(ExportError::InvalidUri)?;
    winsafe::MoveFile(staged_text, destination_text).map_err(|error| {
        let error = io::Error::from_raw_os_error(error.raw() as i32);
        if error.kind() == io::ErrorKind::AlreadyExists {
            ExportError::ExistingDestination
        } else {
            ExportError::Io(error.kind())
        }
    })?;
    Ok(())
}

fn with_owner_acl<R>(
    action: impl FnOnce(&winsafe::SECURITY_ATTRIBUTES<'_>) -> Result<R, ExportError>,
) -> Result<R, ExportError> {
    let sid = current_user_sid()?;
    let sddl = format!("D:P(A;;FA;;;{sid})");
    let parsed: windows_permissions::LocalBox<windows_permissions::SecurityDescriptor> = sddl
        .parse()
        .map_err(|_| ExportError::PermissionRestriction)?;
    let acl = parsed.dacl().ok_or(ExportError::PermissionRestriction)?;
    let mut descriptor =
        winsafe::InitializeSecurityDescriptor().map_err(|_| ExportError::PermissionRestriction)?;
    // Both libraries describe the Win32 ACL pointer. The parsed descriptor
    // owns the ACL for the entire create syscall; neither pointer is
    // dereferenced in this module.
    descriptor.Dacl = std::ptr::from_ref(acl).cast_mut().cast::<winsafe::ACL>();
    descriptor.Control |= co::SE::DACL_PRESENT | co::SE::DACL_PROTECTED;
    if !winsafe::IsValidSecurityDescriptor(&descriptor) {
        return Err(ExportError::PermissionRestriction);
    }
    let mut attributes = winsafe::SECURITY_ATTRIBUTES::default();
    attributes.set_lpSecurityDescriptor(Some(&mut descriptor));
    action(&attributes)
}

pub(super) fn verify_owner_acl(path: &Path) -> Result<(), ExportError> {
    let descriptor = GetNamedSecurityInfo(
        path,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl,
    )
    .map_err(|_| ExportError::PermissionRestriction)?;
    let sddl =
        ConvertSecurityDescriptorToStringSecurityDescriptor(&descriptor, SecurityInformation::Dacl)
            .map_err(|_| ExportError::PermissionRestriction)?;
    let sddl = sddl.to_string_lossy();
    if !sddl.contains("D:P") || sddl.contains("NO_ACCESS_CONTROL") {
        return Err(ExportError::PermissionRestriction);
    }
    let acl = descriptor
        .dacl()
        .ok_or(ExportError::PermissionRestriction)?;
    if acl.len() != 1 {
        return Err(ExportError::PermissionRestriction);
    }
    let ace = acl.get_ace(0).ok_or(ExportError::PermissionRestriction)?;
    let owner = ace.sid().ok_or(ExportError::PermissionRestriction)?;
    if owner.to_string() != current_user_sid()?
        || ace.ace_type() != AceType::ACCESS_ALLOWED_ACE_TYPE
        || !ace.mask().contains(AccessRights::FileAllAccess)
        || ace.flags().contains(AceFlags::Inherited)
    {
        return Err(ExportError::PermissionRestriction);
    }
    Ok(())
}

fn win_error(error: co::ERROR) -> ExportError {
    ExportError::Io(io::Error::from_raw_os_error(error.raw() as i32).kind())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substituted_private_stage_preserves_existing_report_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let parent = tempfile::tempdir()?;
        let replacement = PrivateDirectory::new(parent.path(), "replacement-")?.keep();
        std::fs::write(replacement.join("report"), b"unrelated report")?;
        let destination = parent.path().join("destination.json");
        let created_path = std::cell::RefCell::new(None);
        let result = publish_report_with_created(&destination, b"new payload", |created| {
            *created_path.borrow_mut() = Some(created.to_path_buf());
            std::fs::rename(created, parent.path().join("moved-original-stage"))
                .expect("move the just-created unheld stage");
            std::fs::rename(&replacement, created)
                .expect("substitute an existing protected private directory");
        });
        assert_eq!(result, Err(ExportError::Io(io::ErrorKind::AlreadyExists)));
        let path = created_path.borrow();
        let path = path.as_ref().expect("new stage pathname was created");
        assert_eq!(std::fs::read(path.join("report"))?, b"unrelated report");
        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn protected_null_dacl_is_rejected_without_panicking() -> Result<(), Box<dyn std::error::Error>>
    {
        use windows_permissions::wrappers::SetNamedSecurityInfo;

        let root = tempfile::tempdir()?;
        // Keep the permissive fixture inaccessible through its owner-only parent.
        let stage = PrivateDirectory::new(root.path(), "null-dacl-")?;
        let fixture = stage.path.join("fixture");
        std::fs::write(&fixture, b"")?;
        SetNamedSecurityInfo(
            &fixture,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
            None,
            None,
            None,
            None,
        )?;

        let observed = GetNamedSecurityInfo(
            &fixture,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        )
        .and_then(|descriptor| {
            ConvertSecurityDescriptorToStringSecurityDescriptor(
                &descriptor,
                SecurityInformation::Dacl,
            )
        });
        let verified = std::panic::catch_unwind(|| verify_owner_acl(&fixture));
        // Remove the empty, permissive file before checking either outcome.
        std::fs::remove_file(&fixture)?;

        let sddl = observed?;
        assert!(
            sddl.to_string_lossy().contains("D:P")
                && sddl.to_string_lossy().contains("NO_ACCESS_CONTROL"),
            "fixture must have a protected, present NULL DACL: {sddl:?}"
        );
        assert_eq!(
            verified.expect("valid NULL DACL must not panic"),
            Err(ExportError::PermissionRestriction)
        );
        Ok(())
    }

    #[test]
    fn new_file_has_protected_owner_dacl_while_handle_is_open_before_first_write()
    -> Result<(), Box<dyn std::error::Error>> {
        let parent = tempfile::tempdir()?;
        let helper = winsafe::GetSystemDirectory()?;
        let grant = std::process::Command::new(Path::new(&helper).join("icacls.exe"))
            .arg(parent.path())
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .status()?;
        assert!(
            grant.success(),
            "cannot apply inheritable foreign read fixture"
        );
        let destination = parent.path().join("held-open.json");
        let path = destination.to_str().expect("native test path is Unicode");
        let (file, _) = with_owner_acl(|attributes| {
            winsafe::HFILE::CreateFile(
                path,
                co::GENERIC::WRITE,
                Some(co::FILE_SHARE::READ | co::FILE_SHARE::WRITE),
                Some(attributes),
                co::DISPOSITION::CREATE_NEW,
                co::FILE_ATTRIBUTE::NORMAL,
                None,
                None,
                None,
            )
            .map_err(win_error)
        })?;
        verify_owner_acl(&destination)?;
        assert_eq!(file.WriteFile(b"private")?, 7);
        drop(file);
        assert_eq!(std::fs::read(&destination)?, b"private");
        Ok(())
    }
}
