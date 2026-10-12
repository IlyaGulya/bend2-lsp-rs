use libproc::{
    libproc::proc_pid::pidpath,
    processes::{ProcFilter, pids_by_type},
};
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, fcntl},
    sys::{
        event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue},
        signal::{Signal, kill},
        time::TimeSpec,
    },
    unistd::Pid,
};
use std::{
    fs::{self, File, OpenOptions},
    io,
    os::unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::ExitStatus,
    thread,
    time::{Duration, Instant},
};

pub(crate) struct RecordedTarget {
    pid: u32,
    executable: PathBuf,
    queue: Kqueue,
    status: Option<ExitStatus>,
}

impl RecordedTarget {
    pub(crate) fn discover(executable: &Path) -> io::Result<Option<Self>> {
        let executable = executable.canonicalize()?;
        let mut found = None;
        for pid in pids_by_type(ProcFilter::All).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("Native process enumeration failed: {error}"),
            )
        })? {
            let numeric = i32::try_from(pid).map_err(io::Error::other)?;
            let Ok(path) = pidpath(numeric) else {
                continue;
            };
            if Path::new(&path) == executable && found.replace(pid).is_some() {
                return Err(io::Error::other(
                    "Multiple native targets match the owned executable",
                ));
            }
        }
        let Some(pid) = found else {
            return Ok(None);
        };
        let queue = Kqueue::new()?;
        let registration = KEvent::new(
            usize::try_from(pid).map_err(io::Error::other)?,
            EventFilter::EVFILT_PROC,
            EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_ONESHOT,
            FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS,
            0,
            0,
        );
        queue
            .kevent(
                &[registration],
                &mut [],
                Some(*TimeSpec::from_duration(Duration::ZERO).as_ref()),
            )
            .map_err(|error| {
                io::Error::other(format!(
                    "Native target {pid} exit-watch registration failed: {error}"
                ))
            })?;
        Ok(Some(Self {
            pid,
            executable,
            queue,
            status: None,
        }))
    }

    pub(crate) fn id(&self) -> u32 {
        self.pid
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_some() {
            return Ok(self.status);
        }
        let mut events = [KEvent::new(
            0,
            EventFilter::EVFILT_PROC,
            EvFlags::empty(),
            FilterFlag::empty(),
            0,
            0,
        )];
        let count = self.queue.kevent(
            &[],
            &mut events,
            Some(*TimeSpec::from_duration(Duration::ZERO).as_ref()),
        )?;
        if count == 0 {
            return Ok(None);
        }
        let event = events[0];
        if event.flags().contains(EvFlags::EV_ERROR) {
            return Err(io::Error::from_raw_os_error(
                i32::try_from(event.data()).map_err(io::Error::other)?,
            ));
        }
        if event.ident() != usize::try_from(self.pid).map_err(io::Error::other)?
            || event.filter()? != EventFilter::EVFILT_PROC
            || !event
                .fflags()
                .contains(FilterFlag::NOTE_EXIT | FilterFlag::NOTE_EXITSTATUS)
        {
            return Err(io::Error::other(
                "Native target exit notification lacks exact PID/status evidence",
            ));
        }
        self.status = Some(ExitStatus::from_raw(
            i32::try_from(event.data()).map_err(io::Error::other)?,
        ));
        Ok(self.status)
    }

    pub(super) fn kill(&mut self) -> io::Result<()> {
        if self.try_wait()?.is_some() {
            return Ok(());
        }
        let numeric = i32::try_from(self.pid).map_err(io::Error::other)?;
        if Path::new(&pidpath(numeric).map_err(io::Error::other)?) != self.executable {
            return Err(io::Error::other(
                "Refusing to signal a replaced native target identity",
            ));
        }
        kill(Pid::from_raw(numeric), Signal::SIGKILL)?;
        Ok(())
    }

    pub(super) fn wait(&mut self) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Native target exit not observed within cleanup deadline",
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for RecordedTarget {
    fn drop(&mut self) {
        if self.status.is_none() {
            let _ = self.kill();
            let _ = self.wait();
        }
    }
}

pub(crate) fn open_pipes(
    directory: &Path,
    target: &mut RecordedTarget,
    deadline: Instant,
    mut check_cancelled: impl FnMut() -> io::Result<()>,
) -> io::Result<(File, File)> {
    let stdout = OpenOptions::new()
        .read(true)
        .custom_flags(OFlag::O_NONBLOCK.bits() | OFlag::O_NOFOLLOW.bits())
        .open(directory.join("stdout.fifo"))?;
    let stdin = loop {
        check_cancelled()?;
        if target.try_wait()?.is_some() {
            return Err(io::Error::other(
                "Native target exited before protocol pipe pairing",
            ));
        }
        match OpenOptions::new()
            .write(true)
            .custom_flags(OFlag::O_NONBLOCK.bits() | OFlag::O_NOFOLLOW.bits())
            .open(directory.join("stdin.fifo"))
        {
            Ok(file) => break file,
            Err(error) if error.raw_os_error() == Some(Errno::ENXIO as i32) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Native protocol pipe pairing exceeded capture readiness deadline",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    // Bootstrap opens its stdout writer before its stdin reader. Pairing the
    // stdin writer therefore proves the response writer exists; no held-open
    // response writer or growing-file pseudo-EOF is used.
    for file in [&stdin, &stdout] {
        let flags = OFlag::from_bits_truncate(fcntl(file, FcntlArg::F_GETFL)?);
        fcntl(file, FcntlArg::F_SETFL(flags & !OFlag::O_NONBLOCK))?;
    }
    Ok((stdin, stdout))
}

pub(crate) fn remove_pipes(directory: &Path) -> io::Result<()> {
    for name in ["stdin.fifo", "stdout.fifo"] {
        match fs::remove_file(directory.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
