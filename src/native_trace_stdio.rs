use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use nix::fcntl::{AtFlags, OFlag, openat};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat};
use nix::unistd::{dup2_stderr, dup2_stdin, dup2_stdout};

pub(super) fn initialize() -> io::Result<()> {
    let Some(directory) = std::env::var_os("BEND2_LSP_NATIVE_STDIO_DIR") else {
        return Ok(());
    };
    initialize_at(Path::new(&directory))
}

fn initialize_at(path: &Path) -> io::Result<()> {
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW).bits())
        .open(path)?;

    // Reject incomplete or unsafe endpoints before any blocking open or redirection.
    // All subsequent lookups stay anchored to this opened, non-symlink directory.
    let input_metadata = endpoint_metadata(&directory, "stdin.fifo", SFlag::S_IFIFO)?;
    let output_metadata = endpoint_metadata(&directory, "stdout.fifo", SFlag::S_IFIFO)?;
    let diagnostics_metadata = endpoint_metadata(&directory, "stderr.log", SFlag::S_IFREG)?;

    let diagnostics = open_endpoint(
        &directory,
        "stderr.log",
        OFlag::O_WRONLY | OFlag::O_APPEND,
        &diagnostics_metadata,
    )?;
    dup2_stderr(&diagnostics)?;

    // These blocking, one-way opens are the startup barrier. The parent opens its
    // response reader, then its request writer, only after capture ACK and target
    // exit-watch registration. No runtime or protocol initialization precedes it.
    let output = open_endpoint(&directory, "stdout.fifo", OFlag::O_WRONLY, &output_metadata)?;
    let input = open_endpoint(&directory, "stdin.fifo", OFlag::O_RDONLY, &input_metadata)?;
    dup2_stdout(&output)?;
    dup2_stdin(&input)?;
    Ok(())
}

fn endpoint_metadata(directory: &File, name: &str, kind: SFlag) -> io::Result<FileStat> {
    let metadata = fstatat(directory, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
    if SFlag::from_bits_truncate(metadata.st_mode) & SFlag::S_IFMT != kind {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("native trace endpoint {name} has an invalid file type"),
        ));
    }
    Ok(metadata)
}

fn open_endpoint(
    directory: &File,
    name: &str,
    flags: OFlag,
    expected: &FileStat,
) -> io::Result<File> {
    let file = File::from(openat(
        directory,
        name,
        flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?);
    let actual = fstat(&file)?;
    if actual.st_dev != expected.st_dev
        || actual.st_ino != expected.st_ino
        || actual.st_mode & SFlag::S_IFMT.bits() != expected.st_mode & SFlag::S_IFMT.bits()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("native trace endpoint {name} changed during startup"),
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    #[test]
    fn accepts_only_the_required_endpoint_kinds() -> std::io::Result<()> {
        use super::{File, Mode, SFlag, endpoint_metadata};
        use nix::unistd::mkfifo;
        let temporary = tempfile::tempdir()?;
        let directory = File::open(temporary.path())?;
        for name in ["stdin.fifo", "stdout.fifo"] {
            mkfifo(&temporary.path().join(name), Mode::S_IRUSR | Mode::S_IWUSR)?;
            endpoint_metadata(&directory, name, SFlag::S_IFIFO)?;
            assert!(endpoint_metadata(&directory, name, SFlag::S_IFREG).is_err());
        }
        File::create(temporary.path().join("stderr.log"))?;
        endpoint_metadata(&directory, "stderr.log", SFlag::S_IFREG)?;
        assert!(endpoint_metadata(&directory, "stderr.log", SFlag::S_IFIFO).is_err());
        Ok(())
    }

    #[test]
    fn rejects_missing_and_symlink_endpoints_before_blocking() -> std::io::Result<()> {
        use super::{File, Mode, initialize_at};
        use nix::unistd::mkfifo;
        use std::{fs, os::unix::fs::symlink};
        let temporary = tempfile::tempdir()?;
        for name in ["stdin.fifo", "stdout.fifo"] {
            mkfifo(&temporary.path().join(name), Mode::S_IRUSR | Mode::S_IWUSR)?;
        }
        assert!(initialize_at(temporary.path()).is_err());

        let destination = temporary.path().join("diagnostics");
        File::create(&destination)?;
        symlink(&destination, temporary.path().join("stderr.log"))?;
        assert!(initialize_at(temporary.path()).is_err());
        fs::remove_file(temporary.path().join("stderr.log"))?;
        File::create(temporary.path().join("stderr.log"))?;
        fs::remove_file(temporary.path().join("stdin.fifo"))?;
        symlink(
            temporary.path().join("stdout.fifo"),
            temporary.path().join("stdin.fifo"),
        )?;
        assert!(initialize_at(temporary.path()).is_err());
        Ok(())
    }

    #[test]
    fn rejects_non_directory_and_symlink_roots() -> std::io::Result<()> {
        use super::{File, initialize_at};
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir()?;
        let regular = temporary.path().join("regular");
        File::create(&regular)?;
        assert!(initialize_at(&regular).is_err());
        let linked = temporary.path().join("linked");
        symlink(temporary.path(), &linked)?;
        assert!(initialize_at(&linked).is_err());
        Ok(())
    }
}
