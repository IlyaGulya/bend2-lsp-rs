use super::super::compiler::{CompilerLease, CompilerReapers};
use crate::analysis::DocumentSnapshot;
use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::ChildStdin,
    sync::OwnedSemaphorePermit,
};

const STREAM_LIMIT: u64 = 8 * 1024 * 1024;
const INPUT_LIMIT: usize = 32 * 1024 * 1024;
const REPORT_LIMIT: u64 = 1024 * 1024;
const DETAIL_LIMIT: usize = 16 * 1024;
// A cold native build is not a document query: it gets its own bounded deadline
// and never inherits or raises the ordinary per-check timeout.
const PREPARATION_TIMEOUT: Duration = Duration::from_secs(600);
const SUPPORTED_ANALYZER_VERSION: &str = "0.2.1";

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
        return Err(io::Error::other(
            "bend-lint output exceeds the 8 MiB stream limit",
        ));
    }
    Ok(bytes)
}

// The overlay protocol is an immutable JSON object of paths to texts on stdin.
// Upstream normalizes each key through its own realpath/basename rule, so an
// exact UTF-8 key is required: lossy strings would silently target no file.
fn overlay_key(path: &Path) -> io::Result<String> {
    let text = path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "bend-lint overlay path is not valid UTF-8: {}",
                path.display()
            ),
        )
    })?;
    #[cfg(windows)]
    {
        // Canonical Windows paths carry a verbatim prefix that upstream's
        // Node resolver does not use; normalize its separators as well.
        let normal = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            text.strip_prefix(r"\\?\").unwrap_or(text).to_owned()
        };
        Ok(normal.replace('\\', "/"))
    }
    #[cfg(not(windows))]
    {
        Ok(text.to_owned())
    }
}

struct BoundedOutput {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedOutput {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            exceeded: false,
        }
    }
}

impl io::Write for BoundedOutput {
    fn write(&mut self, chunk: &[u8]) -> io::Result<usize> {
        let next = self.bytes.len().saturating_add(chunk.len());
        if next > self.limit {
            self.exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bend-lint overlay input exceeds the 32 MiB limit",
            ));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(chunk.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn overlay_input(overlays: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>) -> io::Result<Vec<u8>> {
    let mut texts = BTreeMap::<String, &str>::new();
    for (path, snapshot) in overlays {
        let key = overlay_key(path)?;
        match texts.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(snapshot.text.as_str());
            }
            Entry::Occupied(slot) => {
                let key = slot.key();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bend-lint overlay paths collide after normalization: {key}"),
                ));
            }
        }
    }
    // Serialize straight into a capped writer: escaped source text is never
    // materialized past the transport limit, and no second full copy is made.
    let mut writer = BoundedOutput::new(INPUT_LIMIT);
    if let Err(error) = serde_json::to_writer(&mut writer, &texts) {
        if writer.exceeded {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bend-lint overlay input exceeds the 32 MiB limit",
            ));
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bend-lint overlay input could not be encoded: {error}"),
        ));
    }
    Ok(writer.bytes)
}

async fn write_input(stdin: Option<ChildStdin>, input: Option<Vec<u8>>) -> io::Result<()> {
    let Some(mut stdin) = stdin else {
        return if input.is_some() {
            Err(io::Error::other("missing bend-lint stdin"))
        } else {
            Ok(())
        };
    };
    let Some(input) = input else {
        return Err(io::Error::other("unexpected bend-lint stdin"));
    };
    stdin.write_all(&input).await?;
    // Returning drops the only stdin handle, closing the pipe after the
    // complete immutable view; that close is the EOF signal upstream accepts.
    // An explicit shutdown could race a child that already consumed and exited.
    Ok(())
}

fn structured_error(stdout: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(stdout).is_ok_and(|value| {
        value.get("ok") == Some(&serde_json::Value::Bool(false))
            && value
                .get("errors")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|errors| !errors.is_empty())
    })
}

fn spawn_analyzer(
    config: &super::LintConfig,
    directory: &Path,
    arguments: &[OsString],
    wants_stdin: bool,
) -> io::Result<Box<dyn ChildWrapper>> {
    let mut command = CommandWrap::with_new(&config.path, |command| {
        command
            .args(&config.arguments)
            .args(arguments)
            .current_dir(directory)
            .stdin(if wants_stdin {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    command.wrap(KillOnDrop);
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    command.spawn()
}

async fn communicate(
    child: &mut dyn ChildWrapper,
    stdin: Option<ChildStdin>,
    input: Option<Vec<u8>>,
) -> io::Result<std::process::Output> {
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| io::Error::other("missing bend-lint stdout"))?;
    let stderr = child
        .stderr()
        .take()
        .ok_or_else(|| io::Error::other("missing bend-lint stderr"))?;
    // Wrapping the write in Ok keeps BrokenPipe available for deferred
    // structured-error handling, while `try_join` still returns an early
    // collect error (for example an output-size cap) without waiting for a
    // writer blocked on a child that is no longer reading stdin.
    let write = async move { Ok::<_, io::Error>(write_input(stdin, input).await) };
    let collect = async {
        let (status, stdout, stderr) =
            tokio::try_join!(child.wait(), read_pipe(stdout), read_pipe(stderr))?;
        Ok::<_, io::Error>(std::process::Output {
            status,
            stdout,
            stderr,
        })
    };
    let (written, output) = tokio::try_join!(write, collect)?;
    match written {
        Ok(()) => Ok(output),
        // A child may legitimately reject the project before consuming the
        // whole overlay view. Only an attributable structured errors response
        // makes that a reportable result; every other outcome is a
        // truncated-input failure even when the exit status looks clean.
        Err(error)
            if error.kind() == io::ErrorKind::BrokenPipe
                && output.status.code() == Some(2)
                && structured_error(&output.stdout) =>
        {
            Ok(output)
        }
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Err(io::Error::new(
            error.kind(),
            format!(
                "bend-lint closed stdin before receiving the complete overlay input: {error}"
            ),
        )),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("bend-lint did not receive the complete overlay input: {error}"),
        )),
    }
}

// Runs one analyzer invocation with a contained process tree. stdout, stderr
// and stdin are all drained concurrently, so a child that writes while reading
// its overlay cannot deadlock against us.
async fn execute(
    config: &super::LintConfig,
    directory: &Path,
    arguments: Vec<OsString>,
    input: Option<Vec<u8>>,
    timeout: Duration,
    reapers: Arc<CompilerReapers>,
    permit: OwnedSemaphorePermit,
) -> io::Result<std::process::Output> {
    let wants_stdin = input.is_some();
    let lease = reapers.track()?;
    let child = spawn_analyzer(config, directory, &arguments, wants_stdin)?;
    let mut guard = ChildGuard {
        child: Some(child),
        lease: Some(lease),
        permit: Some(permit),
    };
    let child = guard
        .child
        .as_mut()
        .ok_or_else(|| io::Error::other("missing bend-lint child"))?;
    let stdin = child.stdin().take();
    tokio::select! {
        result = tokio::time::timeout(timeout, communicate(child, stdin, input)) => {
            result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "bend-lint timed out; process tree is being killed and reaped"))?
        }
        () = reapers.stopped() => Err(io::Error::new(io::ErrorKind::Interrupted, "bend-lint stopped")),
    }
}

pub(super) async fn run(
    config: &super::LintConfig,
    directory: &Path,
    file: &Path,
    overlays: &BTreeMap<PathBuf, Arc<DocumentSnapshot>>,
    reapers: Arc<CompilerReapers>,
    permit: OwnedSemaphorePermit,
) -> io::Result<std::process::Output> {
    let input = overlay_input(overlays)?;
    let arguments = vec![
        file.as_os_str().to_owned(),
        OsString::from("--json"),
        OsString::from("--overlays"),
        OsString::from("--jobs"),
        OsString::from("1"),
    ];
    execute(
        config,
        directory,
        arguments,
        Some(input),
        Duration::from_millis(config.timeout_ms),
        reapers,
        permit,
    )
    .await
}

pub(super) struct Preparation {
    pub(super) description: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrepareReport {
    bend_lint: String,
    notes: Vec<String>,
    runtime: String,
    platform: String,
    backend: String,
    program: String,
    cache: PrepareCache,
    timings: PrepareTimings,
    exit_code: u64,
}

#[derive(serde::Deserialize)]
struct PrepareCache {
    dir: String,
    built: bool,
    checks: PrepareChecks,
}

#[derive(serde::Deserialize)]
struct PrepareChecks {
    cached: u64,
    checked: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrepareTimings {
    setup_ms: u64,
    checker_ms: u64,
    run_ms: u64,
}

fn bounded(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit.saturating_sub('…'.len_utf8());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

impl PrepareReport {
    fn validate(&self) -> io::Result<()> {
        let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidData, message);
        let version = &self.bend_lint;
        let supported = SUPPORTED_ANALYZER_VERSION;
        if version != supported {
            return Err(invalid(format!(
                "unsupported bend-lint {version} preparation report; this editor requires the {supported} CLI contract"
            )));
        }
        let backend = &self.backend;
        if !matches!(backend.as_str(), "javascript" | "native") {
            return Err(invalid(format!(
                "bend-lint preparation report names an unknown backend '{backend}'"
            )));
        }
        if self.runtime.trim().is_empty()
            || self.platform.trim().is_empty()
            || self.program.trim().is_empty()
        {
            return Err(invalid(
                "bend-lint preparation report omits its runtime, platform or program identity".into(),
            ));
        }
        let exit_code = self.exit_code;
        if exit_code != 0 {
            return Err(invalid(format!(
                "bend-lint preparation report records a nonzero exit code {exit_code}"
            )));
        }
        Ok(())
    }

    fn description(&self) -> String {
        let PrepareReport {
            bend_lint,
            backend,
            runtime,
            platform,
            program,
            cache,
            timings,
            ..
        } = self;
        let cache_dir = bounded(cache.dir.clone(), DETAIL_LIMIT);
        let built = cache.built;
        let cached = cache.checks.cached;
        let checked = cache.checks.checked;
        let setup = timings.setup_ms;
        let checker = timings.checker_ms;
        let run = timings.run_ms;
        let mut description = format!(
            "bend-lint {bend_lint} prepared: backend {backend}, runtime {runtime}, platform {platform}, program {program}, cache {cache_dir} built={built} checks(cached={cached}, checked={checked}), setup {setup} ms, checker {checker} ms, run {run} ms"
        );
        let notes = bounded(self.notes.join("; "), DETAIL_LIMIT);
        if !notes.is_empty() {
            description.push_str("; notes: ");
            description.push_str(&notes);
        }
        description
    }
}

fn read_report(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::metadata(path).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "bend-lint prepare did not write its report at {}: {error}",
                path.display()
            ),
        )
    })?;
    if metadata.len() > REPORT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "bend-lint preparation report exceeds the 1 MiB limit ({} bytes)",
                metadata.len()
            ),
        ));
    }
    fs::read(path).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "bend-lint preparation report could not be read from {}: {error}",
                path.display()
            ),
        )
    })
}

fn preparation_failure(output: &std::process::Output) -> io::Error {
    let detail = super::wire::report_error_detail(&output.stdout)
        .or_else(|| {
            (!output.stderr.is_empty()).then(|| {
                bounded(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                    DETAIL_LIMIT,
                )
            })
        })
        .unwrap_or_else(|| "no diagnostic output".into());
    let status = &output.status;
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("bend-lint prepare failed with {status}: {detail}"),
    )
}

// Builds what a run needs without linting. The report is the only success
// identity: a fresh CLI writes it atomically from a temporary path, so an old
// or different analyzer cannot pass by printing a plausible version.
pub(super) async fn prepare(
    config: &super::LintConfig,
    directory: &Path,
    reapers: Arc<CompilerReapers>,
    permit: OwnedSemaphorePermit,
) -> io::Result<Preparation> {
    let temporary = tempfile::tempdir().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not create a bend-lint preparation workspace: {error}"),
        )
    })?;
    let report = temporary.path().join("report.json");
    let arguments = vec![
        OsString::from("prepare"),
        OsString::from("--json"),
        OsString::from("--report"),
        report.as_os_str().to_owned(),
    ];
    let output = execute(
        config,
        directory,
        arguments,
        None,
        PREPARATION_TIMEOUT,
        reapers,
        permit,
    )
    .await
    .map_err(|error| {
        if error.kind() == io::ErrorKind::TimedOut {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "bend-lint preparation timed out after {} seconds; its process tree is being killed and reaped",
                    PREPARATION_TIMEOUT.as_secs()
                ),
            )
        } else {
            error
        }
    })?;
    if output.status.code() != Some(0) {
        return Err(preparation_failure(&output));
    }
    if !output.stdout.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint prepare returned unexpected stdout; the 0.2.1 contract writes only its report",
        ));
    }
    let bytes = read_report(&report)?;
    let report: PrepareReport = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bend-lint preparation report does not match the 0.2.1 contract: {error}"),
        )
    })?;
    report.validate()?;
    Ok(Preparation {
        description: report.description(),
    })
}
