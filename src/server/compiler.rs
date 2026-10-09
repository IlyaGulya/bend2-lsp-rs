use std::{
    collections::HashMap,
    io::{self, Write as _},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, LazyLock, RwLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Instant, SystemTime},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{Notify, OwnedSemaphorePermit, Semaphore},
};
use tower_lsp::lsp_types::Diagnostic;

use super::features::diag;
use crate::analysis::{self, DocumentSnapshot};
use crate::workspace::{FileId, ImportEdge, SourceGraph, is_hub_import_path};
#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) struct CompilerConfig {
    pub(super) path: String,
    pub(super) arguments: Vec<String>,
}

#[derive(Default)]
pub(super) struct CompilerReapers {
    active: AtomicUsize,
    closed: AtomicBool,
    stopped: Notify,
    idle: Notify,
}

impl CompilerReapers {
    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.stopped.notify_waiters();
    }

    fn track(self: &Arc<Self>) -> io::Result<CompilerLease> {
        // Count admission before observing close, so the final drain cannot miss
        // a child admitted concurrently with shutdown.
        self.active.fetch_add(1, Ordering::SeqCst);
        let lease = CompilerLease(self.clone());
        if self.closed.load(Ordering::SeqCst) {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "compiler checks have been stopped",
            ))
        } else {
            Ok(lease)
        }
    }

    async fn stopped(&self) {
        loop {
            let notified = self.stopped.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.closed.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }

    pub(super) async fn wait(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

struct CompilerLease(Arc<CompilerReapers>);

impl Drop for CompilerLease {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

struct CompilerChild {
    child: Child,
    _resources: Option<(OwnedSemaphorePermit, tempfile::TempDir)>,
    lease: CompilerLease,
}

struct CompilerChildGuard {
    child: Option<CompilerChild>,
}

impl CompilerChildGuard {
    async fn wait_with_output(mut self) -> io::Result<(std::process::Output, Self)> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("compiler child has already been released"))?;
        let output = tokio::select! {
            output = collect_compiler_output(&mut child.child) => output?,
            () = child.lease.0.stopped() => {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "compiler checks have been stopped",
                ));
            }
        };
        Ok((output, self))
    }
}

impl Drop for CompilerChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.child.id().is_none() {
            return;
        }
        // Dispatch cancellation immediately, but retain the child and its resources
        // until wait completes instead of relying on Tokio's best-effort orphan reap.
        let _ = child.child.start_kill();
        tokio::spawn(async move {
            let _ = child.child.wait().await;
            // Dropping the lease notifies the drain only after the child is reaped.
            drop(child);
        });
    }
}

type SourceSnapshot = (PathBuf, Arc<DocumentSnapshot>);
struct StagedImportGraph {
    entry: PathBuf,
    sources: Vec<SourceSnapshot>,
    staged_files: usize,
    staged_bytes: usize,
}

#[derive(Default)]
struct CompilerCheckMetrics {
    root: String,
    cache_hit: Option<bool>,
    staged_files: usize,
    staged_bytes: usize,
    staging_ns: u128,
    child_ns: u128,
    total_ns: u128,
}

fn compiler_metrics_path() -> Option<&'static Path> {
    static PATH: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| std::env::var_os("BEND2_LSP_COMPILER_METRICS_FILE").map(PathBuf::from));
    PATH.as_deref()
}

async fn record_compiler_metrics(path: PathBuf, metrics: CompilerCheckMetrics) {
    static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    let cache = match metrics.cache_hit {
        Some(true) => "hit",
        Some(false) => "miss",
        None => "skip",
    };
    let line = format!(
        "BEND2_COMPILER_METRIC root={} cache={cache} staged_files={} staged_bytes={} staging_ns={} child_ns={} total_ns={}\n",
        metrics.root,
        metrics.staged_files,
        metrics.staged_bytes,
        metrics.staging_ns,
        metrics.child_ns,
        metrics.total_ns,
    );
    let _ = tokio::task::spawn_blocking(move || {
        let _lock = FILE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(line.as_bytes())
    })
    .await;
}

// Cache exact source identities and bytes, not their potentially much larger
// semantic indexes. Borrowed matching avoids allocating a new key on a hit.
struct CompilerSource {
    id: FileId,
    path: PathBuf,
    imports: Box<[ImportEdge]>,
    identity: Weak<DocumentSnapshot>,
    text: Option<Box<str>>,
}

struct CompilerSnapshot {
    compiler_path: String,
    compiler_arguments: Vec<String>,
    compiler_stamp: Option<CompilerStamp>,
    root: FileId,
    sources: Box<[CompilerSource]>,
}

impl CompilerSnapshot {
    fn matches(
        &self,
        config: &CompilerConfig,
        stamp: Option<CompilerStamp>,
        graph: &SourceGraph,
    ) -> bool {
        self.compiler_path == config.path
            && self.compiler_arguments == config.arguments
            && self.compiler_stamp == stamp
            && self.root == graph.root
            && self.sources.len() == graph.nodes.len()
            && self.sources.iter().zip(&graph.nodes).all(|(cached, node)| {
                cached.id == node.id
                    && cached.path == node.path
                    && cached.imports == node.imports
                    && match (cached.text.as_deref(), node.snapshot.as_ref()) {
                        (Some(text), Some(snapshot)) => {
                            cached.identity.as_ptr() == Arc::as_ptr(snapshot)
                                || text == snapshot.text
                        }
                        (None, None) => true,
                        _ => false,
                    }
            })
    }

    fn capture(
        config: CompilerConfig,
        compiler_stamp: Option<CompilerStamp>,
        graph: &SourceGraph,
    ) -> Self {
        Self {
            compiler_path: config.path,
            compiler_arguments: config.arguments,
            compiler_stamp,
            root: graph.root,
            sources: graph
                .nodes
                .iter()
                .map(|node| CompilerSource {
                    id: node.id,
                    path: node.path.clone(),
                    imports: node.imports.clone(),
                    identity: node
                        .snapshot
                        .as_ref()
                        .map_or_else(Weak::new, Arc::downgrade),
                    text: node
                        .snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.text.as_str().into()),
                })
                .collect(),
        }
    }
}

pub(super) struct CachedCompilerResult {
    snapshot: CompilerSnapshot,
    diagnostics: Vec<(PathBuf, Diagnostic)>,
}
impl Default for CompilerConfig {
    fn default() -> Self {
        Self {
            path: "bend".into(),
            arguments: Vec::new(),
        }
    }
}

impl CompilerConfig {
    pub(super) fn from_settings(settings: &serde_json::Value) -> (Self, Vec<String>) {
        let mut config = Self::default();
        let mut warnings = Vec::new();
        let Some(settings) = settings.get("bend2-lsp") else {
            return (config, warnings);
        };
        let Some(settings) = settings.as_object() else {
            warnings.push("bend2-lsp settings must be an object".into());
            return (config, warnings);
        };
        for key in settings.keys() {
            if key != "compilerPath" && key != "compilerArguments" {
                warnings.push(format!("Unknown bend2-lsp setting: {key}"));
            }
        }
        if let Some(value) = settings.get("compilerPath") {
            if let Some(path) = value.as_str().filter(|path| !path.is_empty()) {
                config.path = path.into();
            } else {
                warnings.push("bend2-lsp.compilerPath must be a non-empty string".into());
            }
        }
        if let Some(value) = settings.get("compilerArguments") {
            if let Some(arguments) = value.as_array().and_then(|values| {
                values
                    .iter()
                    .map(serde_json::Value::as_str)
                    .collect::<Option<Vec<_>>>()
            }) {
                config.arguments = arguments.into_iter().map(str::to_owned).collect();
            } else {
                warnings.push("bend2-lsp.compilerArguments must be an array of strings".into());
            }
        }
        (config, warnings)
    }
}

#[derive(Clone)]
pub(super) enum CompilerCompatibility {
    Available { version: String },
    Unknown { message: String },
    Unavailable { message: String },
    Incompatible { message: String },
}

impl CompilerCompatibility {
    pub(super) fn warning(&self) -> Option<&str> {
        match self {
            Self::Available { .. } => None,
            Self::Unknown { message }
            | Self::Unavailable { message }
            | Self::Incompatible { message } => Some(message),
        }
    }

    pub(super) fn failure(&self) -> Option<(&str, &'static str)> {
        match self {
            Self::Unavailable { message } => Some((message, "compiler-unavailable")),
            Self::Incompatible { message } => Some((message, "compiler-incompatible")),
            Self::Available { .. } | Self::Unknown { .. } => None,
        }
    }
}

pub(super) async fn compiler_compatibility(
    config: &CompilerConfig,
    reapers: Arc<CompilerReapers>,
) -> CompilerCompatibility {
    let version = match compiler_probe(config, "version", reapers.clone()).await {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            (output.status.success() && text.to_ascii_lowercase().starts_with("bend"))
                .then_some(text)
        }
        Err(error) => {
            return CompilerCompatibility::Unavailable {
                message: compiler_unavailable_message(config, &error),
            };
        }
    };
    let help = match compiler_probe(config, "--help", reapers).await {
        Ok(output) => output,
        Err(error) => {
            return CompilerCompatibility::Unavailable {
                message: compiler_unavailable_message(config, &error),
            };
        }
    };
    let text = output_text(&help);
    let bend_help = help.status.success()
        && text.to_ascii_lowercase().contains("bend")
        && text.contains("usage:")
        && text.contains("bend <");
    let missing = if !text.contains("--check-only") {
        Some("--check-only")
    } else if !text.contains("bend base") {
        Some("base")
    } else {
        None
    };
    if bend_help && let Some(missing) = missing {
        return CompilerCompatibility::Incompatible {
            message: format!(
                "Bend compiler '{}'{} does not advertise the required {missing} command/option. Select a Bend compiler supporting 'bend <file> --check-only' and 'bend base' with bend2-lsp.compilerPath.",
                config.path,
                version
                    .as_ref()
                    .map_or(String::new(), |version| format!(" ({version})")),
            ),
        };
    }
    if bend_help && let Some(version) = version {
        CompilerCompatibility::Available { version }
    } else {
        CompilerCompatibility::Unknown {
            message: format!(
                "Could not identify the version and CLI contract of Bend compiler '{}'. Compiler checks will still run; verify that it supports 'bend <file> --check-only' and 'bend base'.",
                config.path,
            ),
        }
    }
}

async fn compiler_probe(
    config: &CompilerConfig,
    argument: &str,
    reapers: Arc<CompilerReapers>,
) -> io::Result<std::process::Output> {
    let (output, _child) = run_compiler_command(
        Command::new(&config.path)
            .args(&config.arguments)
            .arg(argument),
        None,
        reapers,
    )
    .await?;
    Ok(output)
}

fn compiler_unavailable_message(config: &CompilerConfig, error: &io::Error) -> String {
    format!(
        "Unable to run Bend compiler '{}': {error}. Install Bend or set bend2-lsp.compilerPath to its executable.",
        config.path,
    )
}

fn output_text(output: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.stdout.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    text
}
#[tracing::instrument(
    name = "compiler.check",
    skip_all,
    fields(
        file_id = tracing::field::debug(&source_graph.root),
        graph_file_count = source_graph.nodes.len(),
        diagnostic_count = tracing::field::Empty,
        cache_hit = tracing::field::Empty,
        outcome = tracing::field::Empty,
    )
)]
pub(super) async fn compiler_diagnostics(
    source_graph: SourceGraph,
    config: CompilerConfig,
    semaphore: Arc<Semaphore>,
    cache: Arc<RwLock<HashMap<PathBuf, CachedCompilerResult>>>,
    reapers: Arc<CompilerReapers>,
) -> Vec<(PathBuf, Diagnostic)> {
    let span = tracing::Span::current();
    let diagnostics = if let Some(path) = compiler_metrics_path() {
        let started = Instant::now();
        let mut metrics = CompilerCheckMetrics::default();
        let diagnostics = compiler_diagnostics_inner(
            source_graph,
            config,
            semaphore,
            cache,
            reapers,
            Some(&mut metrics),
        )
        .await;
        metrics.total_ns = started.elapsed().as_nanos();
        if !metrics.root.is_empty() {
            record_compiler_metrics(path.to_path_buf(), metrics).await;
        }
        diagnostics
    } else {
        compiler_diagnostics_inner(source_graph, config, semaphore, cache, reapers, None).await
    };
    span.record("diagnostic_count", diagnostics.len());
    span.record("outcome", "complete");
    diagnostics
}
async fn compiler_diagnostics_inner(
    source_graph: SourceGraph,
    config: CompilerConfig,
    semaphore: Arc<Semaphore>,
    cache: Arc<RwLock<HashMap<PathBuf, CachedCompilerResult>>>,
    reapers: Arc<CompilerReapers>,
    mut metrics: Option<&mut CompilerCheckMetrics>,
) -> Vec<(PathBuf, Diagnostic)> {
    let span = tracing::Span::current();
    let source_graph = Arc::new(source_graph);
    let Some(root_node) = source_graph.root_node() else {
        return Vec::new();
    };
    let Some(root_snapshot) = root_node.snapshot.clone() else {
        return Vec::new();
    };
    let root = root_node.path.clone();
    if let Some(metrics) = metrics.as_deref_mut() {
        metrics.root = root
            .file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy()
            .into_owned();
    }
    let Ok(permit) = semaphore.acquire_owned().await else {
        return Vec::new();
    };
    let cacheable_sources = cacheable_source_graph(&source_graph)
        && source_graph
            .nodes
            .iter()
            .filter_map(|node| node.snapshot.as_ref())
            .map(|source| source.text.len())
            .fold(0usize, usize::saturating_add)
            <= 1_048_576;
    let compiler_stamp = if cacheable_sources {
        let compiler_path = config.path.clone();
        super::state::blocking_result(
            tokio::task::spawn_blocking(move || compiler_stamp(&compiler_path)).await,
        )
    } else {
        None
    };
    let cacheable = cacheable_sources && compiler_stamp.is_some();
    if let Some(metrics) = metrics.as_deref_mut() {
        metrics.cache_hit = Some(false);
    }
    span.record("cache_hit", false);
    if cacheable {
        let results = super::state::read_lock(&cache);
        if let Some(result) = results.get(&root)
            && result
                .snapshot
                .matches(&config, compiler_stamp, &source_graph)
        {
            if let Some(metrics) = metrics.as_deref_mut() {
                metrics.cache_hit = Some(true);
            }
            span.record("cache_hit", true);
            return result.diagnostics.clone();
        }
    }
    let Some((permit, staging, staged)) =
        stage_compiler_graph(source_graph.clone(), permit, metrics.as_deref_mut()).await
    else {
        return Vec::new();
    };
    let StagedImportGraph { entry, sources, .. } = staged;
    let (output, _child) =
        match run_compiler(&config, entry, permit, staging, reapers, metrics).await {
            Ok(output) => output,
            Err(error) => {
                let message = compiler_unavailable_message(&config, &error);
                return vec![(
                    root,
                    diag(&root_snapshot, 0, 0, message, "compiler-unavailable"),
                )];
            }
        };
    let diagnostics = compiler_output_diagnostics(&root, &root_snapshot, &sources, &output);
    if cacheable {
        let mut results = super::state::write_lock(&cache);
        if !results.contains_key(&root)
            && results.len() >= 32
            && let Some(evicted) = results.keys().next().cloned()
        {
            results.remove(&evicted);
        }
        results.insert(
            root,
            CachedCompilerResult {
                snapshot: CompilerSnapshot::capture(config, compiler_stamp, &source_graph),
                diagnostics: diagnostics.clone(),
            },
        );
    }
    diagnostics
}

async fn stage_compiler_graph(
    source_graph: Arc<SourceGraph>,
    permit: OwnedSemaphorePermit,
    mut metrics: Option<&mut CompilerCheckMetrics>,
) -> Option<(OwnedSemaphorePermit, tempfile::TempDir, StagedImportGraph)> {
    let staging_started = metrics.is_some().then(Instant::now);
    let stage_span = tracing::info_span!(
        "compiler.stage",
        staged_files = tracing::field::Empty,
        staged_bytes = tracing::field::Empty,
        outcome = tracing::field::Empty,
    );
    let worker_span = stage_span.clone();
    let staged = tokio::task::spawn_blocking(move || {
        worker_span.in_scope(|| {
            let staging = tempfile::tempdir().ok()?;
            let staged = stage_source_graph(&source_graph, staging.path())?;
            Some((permit, staging, staged))
        })
    })
    .await;
    if let (Some(metrics), Some(started)) = (metrics.as_deref_mut(), staging_started) {
        metrics.staging_ns = started.elapsed().as_nanos();
    }
    let Ok(Some((permit, staging, staged))) = staged else {
        stage_span.record("outcome", "failed");
        return None;
    };
    stage_span.record("staged_files", staged.staged_files);
    stage_span.record("staged_bytes", staged.staged_bytes);
    stage_span.record("outcome", "complete");
    if let Some(metrics) = metrics {
        metrics.staged_files = staged.staged_files;
        metrics.staged_bytes = staged.staged_bytes;
    }
    Some((permit, staging, staged))
}

#[tracing::instrument(
    name = "compiler.child",
    skip_all,
    fields(outcome = tracing::field::Empty, exit_code = tracing::field::Empty)
)]
async fn run_compiler(
    config: &CompilerConfig,
    entry: PathBuf,
    permit: OwnedSemaphorePermit,
    staging: tempfile::TempDir,
    reapers: Arc<CompilerReapers>,
    metrics: Option<&mut CompilerCheckMetrics>,
) -> io::Result<(std::process::Output, CompilerChildGuard)> {
    let child_started = metrics.is_some().then(Instant::now);
    let output = run_compiler_command(
        Command::new(&config.path)
            .args(&config.arguments)
            .arg(entry)
            .arg("--check-only"),
        Some((permit, staging)),
        reapers,
    )
    .await;
    let span = tracing::Span::current();
    if let (Some(metrics), Some(started)) = (metrics, child_started) {
        metrics.child_ns = started.elapsed().as_nanos();
    }
    match &output {
        Ok((output, _)) if output.status.success() => {
            span.record("outcome", "success");
        }
        Ok((output, _)) => {
            span.record("outcome", "nonzero");
            if let Some(code) = output.status.code() {
                span.record("exit_code", code);
            }
        }
        Err(_) => {
            span.record("outcome", "spawn_error");
        }
    }
    output
}

pub(super) async fn compiler_base(
    config: &CompilerConfig,
    reapers: Arc<CompilerReapers>,
) -> io::Result<std::process::Output> {
    let (output, _child) = run_compiler_command(
        Command::new(&config.path)
            .args(&config.arguments)
            .arg("base"),
        None,
        reapers,
    )
    .await?;
    Ok(output)
}

async fn run_compiler_command(
    command: &mut Command,
    resources: Option<(OwnedSemaphorePermit, tempfile::TempDir)>,
    reapers: Arc<CompilerReapers>,
) -> io::Result<(std::process::Output, CompilerChildGuard)> {
    let lease = reapers.track()?;
    let child = command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    CompilerChildGuard {
        child: Some(CompilerChild {
            child,
            _resources: resources,
            lease,
        }),
    }
    .wait_with_output()
    .await
}

async fn collect_compiler_output(child: &mut Child) -> io::Result<std::process::Output> {
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let (status, stdout, stderr) = tokio::try_join!(
        child.wait(),
        read_compiler_pipe(&mut stdout_pipe),
        read_compiler_pipe(&mut stderr_pipe),
    )?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

async fn read_compiler_pipe<R: AsyncRead + Unpin>(pipe: &mut Option<R>) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    if let Some(pipe) = pipe {
        pipe.read_to_end(&mut output).await?;
    }
    Ok(output)
}

fn compiler_output_diagnostics(
    root: &Path,
    root_snapshot: &DocumentSnapshot,
    sources: &[(PathBuf, Arc<DocumentSnapshot>)],
    output: &std::process::Output,
) -> Vec<(PathBuf, Diagnostic)> {
    if output.status.success() {
        return Vec::new();
    }
    let mut diagnostics = Vec::new();
    for stream in [&output.stderr, &output.stdout] {
        let text = String::from_utf8_lossy(stream);
        let mut block = String::new();
        for line in text.lines() {
            // Bend's expected, observed, Context and Location rows belong
            // to one error. Only its real Error header starts another block.
            if line.starts_with("Error:") && !block.is_empty() {
                append_compiler_error(root, root_snapshot, sources, &block, &mut diagnostics);
                block.clear();
            }
            if line.starts_with("Error:") || !block.is_empty() {
                // Stop this stream's update notice, not the other stream.
                if line.starts_with("bend ") && line.contains(" is available:") {
                    break;
                }
                block.push_str(line);
                block.push('\n');
            }
        }
        if !block.is_empty() {
            append_compiler_error(root, root_snapshot, sources, &block, &mut diagnostics);
        }
    }
    if diagnostics.is_empty() {
        let raw = output_text(output);
        let detail = raw.trim();
        let message = if detail.is_empty() {
            format!(
                "Bend compiler exited with {} without an error message.",
                output.status
            )
        } else {
            detail.to_owned()
        };
        let incompatible = [
            "unknown option --check-only",
            "unrecognized option '--check-only'",
        ]
        .iter()
        .any(|needle| message.contains(needle));
        diagnostics.push((
            root.to_path_buf(),
            diag(
                root_snapshot,
                0,
                0,
                message,
                if incompatible {
                    "compiler-incompatible"
                } else {
                    "checking"
                },
            ),
        ));
    }
    diagnostics
}

fn append_compiler_error(
    root: &Path,
    root_snapshot: &DocumentSnapshot,
    sources: &[SourceSnapshot],
    block: &str,
    diagnostics: &mut Vec<(PathBuf, Diagnostic)>,
) {
    let (detail, location) = block.split_once("\nLocation:").unwrap_or((block, ""));
    let message = detail.strip_prefix("Error:").unwrap_or(detail).trim();
    if message.is_empty() {
        return;
    }
    let excerpts: Vec<_> = location
        .lines()
        .filter_map(parse_compiler_excerpt)
        .collect();
    let marked = excerpts.iter().find(|(_, marked, _)| *marked);
    let mapped = marked.and_then(|(line, _, _)| {
        let mut matches = sources.iter().filter_map(|(source_path, snapshot)| {
            // The CLI does not identify a source path. Validate every available
            // context line before using its line/caret coordinates.
            for (number, _, excerpt) in &excerpts {
                let (start, end) = source_line_range(snapshot, number.checked_sub(1)?)?;
                if snapshot.text.get(start..end)? != *excerpt {
                    return None;
                }
            }
            let (start, end) = source_line_range(snapshot, line.checked_sub(1)?)?;
            let (start, end) =
                compiler_caret_range(snapshot, *line, location).unwrap_or((start, end));
            Some((source_path.as_path(), snapshot.as_ref(), start, end))
        });
        let unique = matches.next()?;
        matches.next().is_none().then_some(unique)
    });
    let (source_path, snapshot, start, end) = mapped.unwrap_or((root, root_snapshot, 0, 0));
    let lowered = message.to_ascii_lowercase();
    let code = if ["import", "file", "hash", "namespace", "cycle", "bend_hub"]
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        "imports"
    } else if lowered.contains("expected : 'def'") {
        "parsing"
    } else {
        "checking"
    };
    let diagnostic = diag(snapshot, start, end, message, code);
    if !diagnostics
        .iter()
        .any(|(path, existing)| path == source_path && *existing == diagnostic)
    {
        diagnostics.push((source_path.to_path_buf(), diagnostic));
    }
}

fn parse_compiler_excerpt(line: &str) -> Option<(usize, bool, &str)> {
    let (prefix, excerpt) = line.split_once('|')?;
    let marked = prefix.ends_with('>');
    let number = prefix.trim_end_matches('>').trim().parse().ok()?;
    Some((number, marked, excerpt.strip_prefix(' ').unwrap_or(excerpt)))
}

fn compiler_caret_range(
    snapshot: &DocumentSnapshot,
    line: usize,
    location: &str,
) -> Option<(usize, usize)> {
    let mut lines = location.lines();
    while let Some(excerpt) = lines.next() {
        let Some((number, true, _)) = parse_compiler_excerpt(excerpt) else {
            continue;
        };
        if number != line {
            continue;
        }
        let (prefix, carets) = lines.next()?.split_once('|')?;
        if !prefix.trim().is_empty() {
            return None;
        }
        let carets = carets.strip_prefix(' ').unwrap_or(carets);
        let first = carets.find('^')?;
        let width = carets[first..].trim_end().len();
        if !carets[..first].bytes().all(|byte| byte == b' ')
            || !carets[first..first + width]
                .bytes()
                .all(|byte| byte == b'^')
        {
            return None;
        }
        let row = u32::try_from(line.checked_sub(1)?).ok()?;
        let start_column = u32::try_from(first).ok()?;
        let end_column = u32::try_from(first.checked_add(width)?).ok()?;
        let start = snapshot
            .line_index
            .offset(&snapshot.text, row, start_column);
        let end = snapshot.line_index.offset(&snapshot.text, row, end_column);
        // Reject out-of-line or mid-surrogate coordinates instead of clamping.
        if snapshot.line_index.position(&snapshot.text, start) != (row, start_column)
            || snapshot.line_index.position(&snapshot.text, end) != (row, end_column)
        {
            return None;
        }
        return Some((start, end));
    }
    None
}

fn cacheable_source_graph(graph: &SourceGraph) -> bool {
    graph.nodes.iter().all(|node| {
        let Some(source) = node.snapshot.as_ref() else {
            return false;
        };
        analysis::imports(source).iter().all(|import| {
            let path = import.path_text(&source.text);
            if path == "Base" || Path::new(path).is_absolute() || is_hub_import_path(path) {
                return false;
            }
            let Some(edge) = node.imports.iter().find(|edge| edge.path == import.path) else {
                return false;
            };
            graph
                .nodes
                .iter()
                .any(|target| target.id == edge.target && target.snapshot.is_some())
        })
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CompilerStamp {
    length: u64,
    modified: SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    permissions: u32,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

pub(super) fn compiler_stamp(path: &str) -> Option<CompilerStamp> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    let executable = Path::new(path);
    let metadata = if executable.is_absolute() || executable.components().count() > 1 {
        compiler_metadata(executable)?
    } else {
        std::env::split_paths(&std::env::var_os("PATH")?)
            .find_map(|directory| compiler_metadata(&directory.join(executable)))?
    };
    Some(CompilerStamp {
        length: metadata.len(),
        modified: metadata.modified().ok()?,
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
        #[cfg(unix)]
        permissions: metadata.mode(),
        #[cfg(not(unix))]
        created: metadata.created().ok(),
    })
}

fn compiler_metadata(path: &Path) -> Option<std::fs::Metadata> {
    let metadata = std::fs::metadata(path)
        .ok()
        .filter(std::fs::Metadata::is_file);
    #[cfg(windows)]
    let metadata = metadata.or_else(|| {
        path.extension()
            .is_none()
            .then(|| path.with_extension("exe"))
            .and_then(|path| std::fs::metadata(path).ok())
            .filter(std::fs::Metadata::is_file)
    });
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        metadata.filter(|metadata| metadata.mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    metadata
}

fn source_line_range(snapshot: &DocumentSnapshot, wanted: usize) -> Option<(usize, usize)> {
    let range = snapshot.line_index.line_range(&snapshot.text, wanted)?;
    Some((range.start, range.end))
}
fn stage_path(root: &Path, staging: &Path) -> PathBuf {
    let mut result = staging.to_path_buf();
    for component in root.components() {
        match component {
            Component::Prefix(prefix) => {
                result.push(prefix.as_os_str().to_string_lossy().replace(':', "_"));
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => result.push("_parent"),
            Component::Normal(part) => result.push(part),
        }
    }
    result
}
fn stage_source_graph(graph: &SourceGraph, staging: &Path) -> Option<StagedImportGraph> {
    let root = graph.root_node()?;
    let mut sources = Vec::with_capacity(graph.nodes.len());
    let mut staged_bytes = 0usize;
    for node in &graph.nodes {
        let Some(snapshot) = &node.snapshot else {
            continue;
        };
        let staged = stage_path(&node.path, staging);
        std::fs::create_dir_all(staged.parent()?).ok()?;
        std::fs::write(&staged, &snapshot.text).ok()?;
        staged_bytes = staged_bytes.saturating_add(snapshot.text.len());
        sources.push((node.path.clone(), snapshot.clone()));
    }
    Some(StagedImportGraph {
        entry: stage_path(&root.path, staging),
        staged_files: sources.len(),
        staged_bytes,
        sources,
    })
}

#[cfg(all(test, unix))]
mod snapshot_lifetime_tests {
    use super::{CompilerConfig, CompilerReapers, compiler_diagnostics};
    use crate::{
        analysis::Revision,
        workspace::{Document, WorkspaceDb},
    };
    use std::{
        collections::HashMap,
        sync::{Arc, RwLock},
    };
    use tokio::sync::Semaphore;
    use url::Url;

    #[tokio::test]
    async fn completed_compiler_cache_does_not_retain_semantic_snapshots()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("main.bend");
        let uri = Url::from_file_path(&path).map_err(|()| std::io::Error::other("source URI"))?;
        let document = Document::new(
            uri,
            "bend".into(),
            Revision(1),
            "def main: U32\n  1\n".into(),
        );
        let weak = Arc::downgrade(&document.snapshot);
        let mut database = WorkspaceDb::default();
        let root = database.set_open_document(document, Some(path));
        let graph = database
            .source_graph(root)
            .ok_or_else(|| std::io::Error::other("source graph"))?;
        let cache = Arc::new(RwLock::new(HashMap::new()));
        let reapers = Arc::new(CompilerReapers::default());
        let diagnostics = compiler_diagnostics(
            graph,
            CompilerConfig {
                path: "/usr/bin/true".into(),
                arguments: Vec::new(),
            },
            Arc::new(Semaphore::new(1)),
            cache.clone(),
            reapers.clone(),
        )
        .await;
        assert!(
            diagnostics.is_empty(),
            "the real successful child must not report compiler failures"
        );
        reapers.close();
        reapers.wait().await;
        drop(database);
        assert!(
            weak.upgrade().is_none(),
            "completed compiler results must not own full semantic snapshots"
        );
        drop(cache);
        Ok(())
    }
}
