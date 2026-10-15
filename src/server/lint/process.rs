use super::super::compiler::{CompilerLease, CompilerReapers};
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::{io, path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{io::{AsyncRead, AsyncReadExt}, sync::OwnedSemaphorePermit};

const STREAM_LIMIT: u64 = 8 * 1024 * 1024;

// Like compiler checks, cancelled jobs retain their concurrency lease until
// the child has actually been killed and reaped. Containment includes the
// analyzer's compiler, rule workers and JavaScript runtime children.
struct ChildGuard {
    child: Option<Box<dyn ChildWrapper>>,
    lease: Option<CompilerLease>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let lease = self.lease.take();
            let permit = self.permit.take();
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
                drop((lease, permit));
            });
        }
    }
}

async fn read_pipe(pipe: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.take(STREAM_LIMIT + 1).read_to_end(&mut bytes).await?;
    if u64::try_from(bytes.len()).map_err(io::Error::other)? > STREAM_LIMIT {
        return Err(io::Error::other("bend-lint output exceeds the 8 MiB stream limit"));
    }
    Ok(bytes)
}

pub(super) async fn run(
    config: &super::LintConfig,
    directory: &Path,
    file: &Path,
    reapers: Arc<CompilerReapers>,
    permit: OwnedSemaphorePermit,
) -> io::Result<std::process::Output> {
    let lease = reapers.track()?;
    let mut command = CommandWrap::with_new(&config.path, |command| {
        command.args(&config.arguments).arg(file).args(["--json", "--jobs", "1"])
            .current_dir(directory).stdin(Stdio::null())
            .stdout(Stdio::piped()).stderr(Stdio::piped());
    });
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    let child = command.spawn()?;
    let mut guard = ChildGuard { child: Some(child), lease: Some(lease), permit: Some(permit) };
    let child = guard.child.as_mut().ok_or_else(|| io::Error::other("missing bend-lint child"))?;
    let stdout = child.stdout().take().ok_or_else(|| io::Error::other("missing bend-lint stdout"))?;
    let stderr = child.stderr().take().ok_or_else(|| io::Error::other("missing bend-lint stderr"))?;
    let collect = async {
        let (status, stdout, stderr) = tokio::try_join!(child.wait(), read_pipe(stdout), read_pipe(stderr))?;
        Ok(std::process::Output { status, stdout, stderr })
    };
    tokio::select! {
        result = tokio::time::timeout(Duration::from_millis(config.timeout_ms), collect) => {
            result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "bend-lint timed out; process tree is being killed and reaped"))?
        }
        () = reapers.stopped() => Err(io::Error::new(io::ErrorKind::Interrupted, "bend-lint stopped")),
    }
}
