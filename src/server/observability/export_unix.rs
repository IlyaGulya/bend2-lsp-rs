//! Descriptor-relative staging and no-clobber publication on Unix.
//!
//! The parent and staging directory stay open. A writable parent may move the
//! staging directory entry, but cannot make descriptor-relative publication read a different source
//! from the authenticated directory descriptor.

use super::{ExportError, stage_name};
use rustix::fs::{self, AtFlags, Mode, OFlags};
use std::{fs::File, io::{self, Write}, path::{Path, PathBuf}};

const STAGED_FILE: &str = "report";

struct PrivateDirectory {
    parent: File,
    directory: File,
    name: String,
    path: PathBuf,
    cleanup: bool,
    created_report: bool,
}

impl PrivateDirectory {
    fn new(parent: &Path, prefix: &str) -> Result<Self, ExportError> {
        Self::new_with_created(parent, prefix, |_| {})
    }

    // Test seam: a parent-entry writer may move the new directory after
    // mkdirat and before we acquire its descriptor.
    fn new_with_created(
        parent: &Path,
        prefix: &str,
        on_created: impl FnOnce(&Path),
    ) -> Result<Self, ExportError> {
        let parent_fd = File::open(parent).map_err(io_error)?;
        Self::new_from_parent(parent, parent_fd, prefix, on_created)
    }

    fn new_from_parent(
        parent: &Path,
        parent_fd: File,
        prefix: &str,
        on_created: impl FnOnce(&Path),
    ) -> Result<Self, ExportError> {
        for _ in 0..16 {
            let name = stage_name(prefix);
            match fs::mkdirat(&parent_fd, name.as_str(), Mode::RWXU) {
                Ok(()) => {}
                Err(error) if error == rustix::io::Errno::EXIST => continue,
                Err(error) => return Err(io_error(error.into())),
            }
            on_created(&parent.join(&name));
            let descriptor = fs::openat(
                &parent_fd, name.as_str(),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            );
            let directory = match descriptor {
                Ok(descriptor) => File::from(descriptor),
                Err(error) => {
                    // Only an empty entry is removed; never recursively walk
                    // a pathname which another writer might have replaced.
                    let _ = fs::unlinkat(&parent_fd, name.as_str(), AtFlags::REMOVEDIR);
                    return Err(io_error(error.into()));
                }
            };
            let stage = Self {
                parent: parent_fd,
                directory,
                path: parent.join(&name),
                name,
                cleanup: true,
                created_report: false,
            };
            stage.restrict_directory()?;
            return Ok(stage);
        }
        Err(ExportError::Io(io::ErrorKind::AlreadyExists))
    }

    fn restrict_directory(&self) -> Result<(), ExportError> {
        let stat = fs::fstat(&self.directory).map_err(|error| io_error(error.into()))?;
        if stat.st_uid != rustix::process::geteuid().as_raw() ||
            (stat.st_mode & 0o170000) != 0o040000 {
            return Err(ExportError::PermissionRestriction);
        }
        fs::fchmod(&self.directory, Mode::RWXU)
            .map_err(|error| io_error(error.into()))?;
        #[cfg(target_os = "macos")]
        restrict_extended_acl(&self.directory)?;
        let stat = fs::fstat(&self.directory).map_err(|error| io_error(error.into()))?;
        if stat.st_mode & 0o777 != 0o700 {
            return Err(ExportError::PermissionRestriction);
        }
        Ok(())
    }

    fn keep(mut self) -> PathBuf {
        self.cleanup = false;
        self.path.clone()
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        if self.cleanup {
            // Both unlinks use already-open directories. A replaced staging
            // pathname cannot make cleanup traverse or delete its contents.
            if self.created_report {
                // EXCL creation succeeded in this held directory. A foreign
                // report already present after a substituted directory is
                // never our file to unlink.
                let _ = fs::unlinkat(&self.directory, STAGED_FILE, AtFlags::empty());
            }
            let _ = fs::unlinkat(&self.parent, self.name.as_str(), AtFlags::REMOVEDIR);
        }
    }
}

pub(super) fn private_directory(parent: &Path, prefix: &str) -> Result<PathBuf, ExportError> {
    #[cfg(target_os = "macos")]
    {
        let parent_fd = File::open(parent).map_err(io_error)?;
        verify_noarg_parent(&parent_fd)?;
        return Ok(PrivateDirectory::new_from_parent(parent, parent_fd, prefix, |_| {})?.keep());
    }
    #[cfg(not(target_os = "macos"))]
    Ok(PrivateDirectory::new(parent, prefix)?.keep())
}

#[cfg(target_os = "macos")]
fn verify_noarg_parent(parent: &File) -> Result<(), ExportError> {
    use std::os::fd::AsFd;
    let stat = fs::fstat(parent).map_err(|error| io_error(error.into()))?;
    // The noarg directory itself must be private from the moment it is
    // created. A foreign-inheritable ACE on TMPDIR cannot be removed before
    // mkdirat, unlike the empty staging directory used by explicit exports.
    if (stat.st_mode & 0o170000) != 0o040000 ||
        (stat.st_uid != rustix::process::geteuid().as_raw() && stat.st_uid != 0) ||
        stat.st_mode & 0o022 != 0 {
        return Err(ExportError::PermissionRestriction);
    }
    let acl = calcifer_macos_acl::read_acl(parent.as_fd())
        .map_err(|_| ExportError::PermissionRestriction)?;
    if !acl.is_empty() {
        return Err(ExportError::PermissionRestriction);
    }
    Ok(())
}

pub(super) fn publish_report(destination: &Path, payload: &[u8]) -> Result<(), ExportError> {
    publish_report_with_created(destination, payload, |_| {})
}

fn publish_report_with_created(
    destination: &Path,
    payload: &[u8],
    on_created: impl FnOnce(&Path),
) -> Result<(), ExportError> {
    let parent = destination.parent().ok_or(ExportError::ParentNotDirectory)?;
    let name = destination.file_name().ok_or(ExportError::InvalidUri)?;
    let mut stage = PrivateDirectory::new_with_created(parent, ".bend2-diagnostic-report-", on_created)?;
    let fd = fs::openat(
        &stage.directory, STAGED_FILE,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    ).map_err(|error| io_error(error.into()))?;
    stage.created_report = true;
    let mut file = File::from(fd);
    fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
        .map_err(|error| io_error(error.into()))?;
    #[cfg(target_os = "macos")]
    restrict_extended_acl(&file)?;
    let stat = fs::fstat(&file).map_err(|error| io_error(error.into()))?;
    if stat.st_mode & 0o777 != 0o600 || stat.st_uid != rustix::process::geteuid().as_raw() ||
        (stat.st_mode & 0o170000) != 0o100000 {
        return Err(ExportError::PermissionRestriction);
    }
    file.write_all(payload).map_err(io_error)?;
    file.flush().map_err(io_error)?;
    // No path resolution of the source or destination parent: the source
    // remains the file in `stage.directory` even after parent-entry renames.
    // Unlike hard links, no-replace rename also works on filesystems that do
    // not support multiple links to the same file.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let publication = fs::renameat_with(
        &stage.directory, STAGED_FILE, &stage.parent, name, fs::RenameFlags::NOREPLACE,
    );
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let publication = fs::linkat(
        &stage.directory, STAGED_FILE, &stage.parent, name, AtFlags::empty(),
    );
    publication.map_err(|error| {
        let error: io::Error = error.into();
        if error.kind() == io::ErrorKind::AlreadyExists {
            ExportError::ExistingDestination
        } else {
            io_error(error)
        }
    })?;
    Ok(())
}

fn io_error(error: io::Error) -> ExportError {
    ExportError::Io(error.kind())
}

#[cfg(target_os = "macos")]
fn restrict_extended_acl(file: &File) -> Result<(), ExportError> {
    use std::os::fd::AsFd;
    // Operate on the held vnode, never on a pathname that another process
    // could substitute between clearing and checking the ACL.
    calcifer_macos_acl::clear_acl(file.as_fd())
        .map_err(|_| ExportError::PermissionRestriction)?;
    let acl = calcifer_macos_acl::read_acl(file.as_fd())
        .map_err(|_| ExportError::PermissionRestriction)?;
    if !acl.is_empty() {
        return Err(ExportError::PermissionRestriction);
    }
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substituted_private_stage_never_deletes_an_existing_report()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::DirBuilderExt;

        let parent = tempfile::tempdir()?;
        let replacement = parent.path().join("private-replacement");
        std::fs::DirBuilder::new().mode(0o700).create(&replacement)?;
        let original = replacement.join(STAGED_FILE);
        std::fs::write(&original, b"unrelated report")?;
        let destination = parent.path().join("destination.json");

        let staged = std::cell::RefCell::new(None);
        let result = publish_report_with_created(&destination, b"new payload", |created| {
            *staged.borrow_mut() = Some(created.to_path_buf());
            std::fs::rename(created, parent.path().join("moved-original-stage"))
                .expect("move just-created stage before it opens");
            std::fs::rename(&replacement, created)
                .expect("put existing owner-private directory in stage position");
        });
        assert_eq!(result, Err(ExportError::Io(io::ErrorKind::AlreadyExists)));
        let stage_path = staged.borrow();
        let stage_path = stage_path.as_ref().expect("stage entry was created");
        assert_eq!(std::fs::read(stage_path.join(STAGED_FILE))?, b"unrelated report");
        assert!(!destination.exists());
        Ok(())
    }
}
