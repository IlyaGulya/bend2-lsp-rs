mod actions;
mod config;
pub(super) mod control;
mod process;
mod results;
mod snapshot;
mod wire;
pub(super) use actions::DANGEROUS_KIND;
use config::LintConfig;
use control::{LintControl, LintPhase};
use results::LintResults;

use super::{
    lsp::Backend,
    state::{State, revision_result},
};
use crate::workspace::{Document, FileId};
use std::{
    collections::{BTreeSet, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Semaphore, watch},
    task::JoinHandle,
};
use tower_lsp::lsp_types::MessageType;
use url::Url;

// Long enough to absorb a burst of keystrokes, short enough that a stopped
// editor sees refreshed diagnostics without an explicit command.
const LINT_DEBOUNCE: Duration = Duration::from_millis(250);

pub(super) struct LintResult {
    epoch: u64,
    revisions: Vec<(FileId, u64)>,
    config: LintConfig,
    snapshot: Arc<snapshot::ProjectSnapshot>,
    findings: wire::Findings,
}

#[derive(Clone, PartialEq, Eq)]
struct PreparationKey {
    root: PathBuf,
    config: LintConfig,
    inputs: [u8; 32],
}

enum PreparationPhase {
    Running,
    Ready(String),
    Failed(String),
}

// Preparation belongs to the project/analyzer configuration, never to a
// document revision: typing reuses an in-flight or completed preparation and
// only configuration, trust, or project identity changes can restart it.
struct Preparation {
    key: PreparationKey,
    phase: PreparationPhase,
    task: Option<JoinHandle<()>>,
}

pub(super) struct LintService {
    config: State<LintConfig>,
    results: State<LintResults>,
    control: LintControl,
    task: State<Option<JoinHandle<()>>>,
    pending: State<BTreeSet<Url>>,
    active: State<Option<Url>>,
    preparation: State<Option<Preparation>>,
    preparation_changed: watch::Sender<u64>,
    prep_semaphore: Arc<Semaphore>,
    epoch: AtomicU64,
    progress: AtomicU64,
    closed: AtomicBool,
    semaphore: Arc<Semaphore>,
}

pub(super) struct LintInvalidation {
    previous: Option<JoinHandle<()>>,
    affected: HashSet<Url>,
    interrupted: Vec<Url>,
    warning: Option<String>,
}

impl LintInvalidation {
    // A selective invalidation aborts the scheduler: results matched by the
    // caller are gone, and work that was queued or running around them is
    // handed back so the caller can re-schedule it instead of losing it.
    pub(super) fn reschedule(&self) -> Vec<Url> {
        self.affected
            .iter()
            .cloned()
            .chain(self.interrupted.iter().cloned())
            .collect()
    }
}

impl Default for LintService {
    fn default() -> Self {
        let (preparation_changed, _) = watch::channel(0u64);
        Self {
            config: State::new(LintConfig::default()),
            results: State::new(LintResults::default()),
            control: LintControl::default(),
            task: State::new(None),
            pending: State::new(BTreeSet::new()),
            active: State::new(None),
            preparation: State::new(None),
            preparation_changed,
            prep_semaphore: Arc::new(Semaphore::new(1)),
            epoch: AtomicU64::new(0),
            progress: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            semaphore: Arc::new(Semaphore::new(1)),
        }
    }
}

impl LintService {
    fn next_epoch(&self) -> u64 {
        self.epoch
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                epoch.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("bend-lint epoch exhausted"))
            + 1
    }

    fn next_progress(&self) -> u64 {
        self.progress.fetch_add(1, Ordering::SeqCst)
    }

    fn take_results(&self) -> HashSet<Url> {
        self.results.write().clear()
    }

    fn invalidate(&self, config: Option<LintConfig>, warning: Option<String>) -> LintInvalidation {
        let mut task = self.task.write();
        if let Some(config) = config {
            self.control.configure(&config, warning.clone());
            *self.config.write() = config;
            self.cancel_preparation();
        } else {
            self.control
                .invalidate("Project inputs changed; the next check will re-read them");
        }
        self.next_epoch();
        self.pending.write().clear();
        self.active.write().take();
        let previous = task.take();
        if let Some(previous) = &previous {
            previous.abort();
        }
        LintInvalidation {
            previous,
            affected: self.take_results(),
            interrupted: Vec::new(),
            warning,
        }
    }

    fn invalidate_entries(
        &self,
        mut matches: impl FnMut(&Url, &LintResult) -> bool,
    ) -> LintInvalidation {
        let mut task = self.task.write();
        self.next_epoch();
        // Hand back every request the aborted scheduler owned. Clearing them
        // here dropped work whenever the selective invalidation matched no
        // finished result, so a pending or preparing entry was never re-run.
        let interrupted: Vec<Url> = {
            let mut pending = self.pending.write();
            let mut interrupted: Vec<Url> = pending.iter().cloned().collect();
            pending.clear();
            if let Some(active) = self.active.write().take() {
                interrupted.push(active);
            }
            interrupted
        };
        let previous = task.take();
        if let Some(previous) = &previous {
            previous.abort();
        }
        self.control
            .invalidate_running("Inputs changed; the affected results will be rechecked");
        let affected = self.results.write().invalidate(|uri, result| {
            if !matches(uri, result) {
                return false;
            }
            self.control.record(
                uri,
                LintPhase::Waiting,
                "Inputs changed; this result will be rechecked",
            );
            true
        });
        LintInvalidation {
            previous,
            affected,
            interrupted,
            warning: None,
        }
    }

    fn cancel_preparation(&self) {
        let mut state = self.preparation.write();
        if let Some(entry) = state.take()
            && let Some(task) = entry.task
        {
            task.abort();
        }
        drop(state);
        self.preparation_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn reset_failed_preparation(&self) {
        let mut state = self.preparation.write();
        if !state
            .as_ref()
            .is_some_and(|entry| matches!(&entry.phase, PreparationPhase::Failed(_)))
        {
            return;
        }
        if let Some(entry) = state.take()
            && let Some(task) = entry.task
        {
            task.abort();
        }
        drop(state);
        self.preparation_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

impl Backend {
    pub(super) fn configure_lint(&self, settings: &serde_json::Value) -> LintInvalidation {
        let parsed = LintConfig::from_settings(settings);
        let warning = parsed
            .as_ref()
            .err()
            .map(|message| format!("bend-lint configuration error: {message}"));
        self.diagnostics
            .lint
            .invalidate(Some(parsed.unwrap_or_default()), warning)
    }

    pub(super) fn invalidate_lint(&self) -> LintInvalidation {
        self.diagnostics.lint.invalidate(None, None)
    }

    pub(super) fn close_lint(&self, uri: &Url) -> LintInvalidation {
        let identities: Vec<PathBuf> = uri
            .to_file_path()
            .ok()
            .into_iter()
            .flat_map(|path| [Some(path.clone()), snapshot::input_identity(&path)])
            .flatten()
            .collect();
        let invalidation = self.diagnostics.lint.invalidate_entries(|_, result| {
            identities
                .iter()
                .any(|path| result.snapshot.covers_path(path))
        });
        self.diagnostics.lint.control.forget(uri);
        invalidation
    }

    // Lifecycle callers biased-join their original mutation before this cleanup:
    // no lint await may precede its revision reservation or operation fence.
    pub(super) async fn finish_lint_invalidation(&self, invalidation: LintInvalidation) {
        join_lint_task(invalidation.previous).await;
        self.end_all_lint_progress("Cancelled; the current inputs will be rechecked")
            .await;
        if !invalidation.affected.is_empty() {
            self.publish_coordinated_diagnostics(invalidation.affected)
                .await;
        }
        if let Some(warning) = invalidation.warning {
            self.client
                .log_message(MessageType::WARNING, &warning)
                .await;
            self.client
                .show_message(MessageType::WARNING, warning)
                .await;
        }
    }

    pub(super) async fn shutdown_lint(&self) {
        self.diagnostics.lint.closed.store(true, Ordering::SeqCst);
        self.diagnostics.lint.semaphore.close();
        self.diagnostics.lint.prep_semaphore.close();
        self.diagnostics.lint.cancel_preparation();
        self.finish_lint_invalidation(self.invalidate_lint()).await;
    }

    pub(super) fn current_lint_result(&self, uri: &Url) -> Option<Arc<LintResult>> {
        let result = self.diagnostics.lint.results.read().get(uri)?;
        (*self.diagnostics.lint.config.read() == result.config
            && self.lint_revisions_current(&result.revisions))
        .then_some(result)
    }

    fn lint_revisions_current(&self, revisions: &[(FileId, u64)]) -> bool {
        let state = self.workspace.state.read();
        revisions.iter().all(|(id, generation)| {
            state.revisions.get(id).is_some_and(|sync| {
                let status = revision_result(sync.status());
                !status.is_pending() && status.generation() == *generation
            })
        })
    }

    fn lint_revision_fences(
        &self,
        snapshot: &snapshot::ProjectSnapshot,
        documents: &[Document],
    ) -> Vec<(FileId, u64)> {
        let state = self.workspace.state.read();
        documents
            .iter()
            .filter_map(|document| {
                let path = document.uri.to_file_path().ok()?;
                if !snapshot.covers_path(&path)
                    && !snapshot::input_identity(&path)
                        .is_some_and(|identity| snapshot.covers_path(&identity))
                {
                    return None;
                }
                let id = state.database.file_id_by_uri(&document.uri)?;
                let sync = state.revisions.get(&id)?;
                Some((id, revision_result(sync.status()).generation()))
            })
            .collect()
    }

    // A project snapshot overlays every open input, so a change anywhere in a
    // checked project invalidates every retained entry in it. Refresh them
    // together; import dependents alone would leave independent entries with a
    // stale view whose fixes never become available again.
    pub(super) fn lint_affected_documents(&self, uris: impl IntoIterator<Item = Url>) -> Vec<Url> {
        let mut requested: BTreeSet<Url> = uris.into_iter().collect();
        let identities: Vec<PathBuf> = requested
            .iter()
            .filter_map(|uri| uri.to_file_path().ok())
            .flat_map(|path| [Some(path.clone()), snapshot::input_identity(&path)])
            .flatten()
            .collect();
        requested.extend(self.diagnostics.lint.results.read().covering(&identities));
        requested.into_iter().collect()
    }

    pub(super) fn lint_scheduling_enabled(&self) -> bool {
        self.diagnostics.lint.config.read().enabled
            && !self.diagnostics.lint.closed.load(Ordering::SeqCst)
    }

    pub(super) fn lint_diagnostics(&self, uri: &Url) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        self.current_lint_result(uri)
            .and_then(|result| result.findings.by_uri.get(uri).cloned())
            .unwrap_or_default()
    }

    pub(super) fn lint_tool_identity(&self, config: &LintConfig, uri: &Url) -> String {
        let root = uri
            .to_file_path()
            .ok()
            .and_then(|path| snapshot::project_root(&path));
        let preparation = self.diagnostics.lint.preparation.read();
        match preparation.as_ref() {
            Some(entry) if entry.key.config == *config && Some(&entry.key.root) == root.as_ref() => {
                match &entry.phase {
                    PreparationPhase::Ready(description) => description.clone(),
                    PreparationPhase::Running => "Preparing the analyzer and its pinned compiler; the identity is reported when preparation completes.".into(),
                    PreparationPhase::Failed(message) => message.clone(),
                }
            }
            _ => "Not prepared yet; the first check validates the analyzer contract and reports its identity.".into(),
        }
    }

    // Schedule a debounced or immediate check for the given open documents.
    // Requests accumulate while one debounce window or check is in flight, and
    // the newest request always supersedes stale work.
    pub(super) async fn schedule_lint(&self, uris: impl IntoIterator<Item = Url>, debounce: bool) {
        // Keep the default-disabled provider out of the typing hot path: no
        // document clones or dependent lookups before this check.
        let enabled = self.diagnostics.lint.config.read().enabled;
        if !enabled || self.diagnostics.lint.closed.load(Ordering::SeqCst) {
            return;
        }
        let mut requested = BTreeSet::new();
        for uri in uris {
            if self.document(&uri).as_ref().is_some_and(Self::supported) {
                requested.insert(uri);
            }
        }
        if requested.is_empty() {
            return;
        }
        {
            let mut task = self.diagnostics.lint.task.write();
            {
                let mut pending = self.diagnostics.lint.pending.write();
                pending.extend(requested);
                if let Some(active) = self.diagnostics.lint.active.write().take() {
                    // Superseding a running check must not lose its entry.
                    pending.insert(active);
                }
            }
            let epoch = self.diagnostics.lint.next_epoch();
            let previous = task.take();
            if let Some(previous) = &previous {
                previous.abort();
            }
            let backend = self.clone();
            let delay = debounce.then_some(LINT_DEBOUNCE);
            let handle = tokio::spawn(async move {
                let reporter = backend.clone();
                let operation = async move {
                    join_lint_task(previous).await;
                    if let Some(delay) = delay {
                        tokio::time::sleep(delay).await;
                    }
                    backend.run_scheduled_lint(epoch).await;
                };
                super::state::supervise(operation, move || reporter.diagnostics.report_fatal()).await;
            });
            *task = Some(handle);
        }
        self.diagnostics
            .lint
            .control
            .invalidate_running("Superseded by newer editor changes");
        self.end_all_lint_progress("Superseded by newer editor changes")
            .await;
    }

    async fn run_scheduled_lint(&self, epoch: u64) {
        loop {
            if self.diagnostics.lint.closed.load(Ordering::SeqCst)
                || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
            {
                return;
            }
            let Some(uri) = self.diagnostics.lint.pending.write().pop_first() else {
                return;
            };
            if self.diagnostics.lint.closed.load(Ordering::SeqCst)
                || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
            {
                // A newer request owns this entry now; never drop it.
                self.diagnostics.lint.pending.write().insert(uri);
                return;
            }
            *self.diagnostics.lint.active.write() = Some(uri.clone());
            if let Some(document) = self.document(&uri).filter(Self::supported) {
                let config = self.diagnostics.lint.config.read().clone();
                if self.diagnostics.lint.epoch.load(Ordering::SeqCst) == epoch {
                    self.run_lint(document, config, epoch).await;
                } else {
                    self.diagnostics.lint.pending.write().insert(uri.clone());
                    return;
                }
            }
            {
                let mut active = self.diagnostics.lint.active.write();
                if active.as_ref().is_some_and(|active| *active == uri) {
                    active.take();
                }
            }
        }
    }

    async fn run_lint(&self, document: Document, config: LintConfig, epoch: u64) {
        let uri = document.uri.clone();
        let progress = self.begin_lint_progress(&uri).await;
        let reason = match self.execute_lint(document, config, epoch).await {
            Ok(reason) => reason,
            Err(error) => {
                let phase = if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::NotFound
                ) {
                    LintPhase::Skipped
                } else {
                    LintPhase::Failed
                };
                let message = error.to_string();
                self.finish_lint_control(&uri, phase, &message, epoch)
                    .await;
                message
            }
        };
        self.end_lint_progress(progress, &reason).await;
    }

    async fn execute_lint(
        &self,
        document: Document,
        config: LintConfig,
        epoch: u64,
    ) -> std::io::Result<String> {
        let uri = document.uri.clone();
        let Some(file) = uri.to_file_path().ok() else {
            self.finish_lint_control(
                &uri,
                LintPhase::Skipped,
                "Save this document to a file before checking",
                epoch,
            )
            .await;
            return Ok("Analysis skipped; the document has no file path".into());
        };
        let documents = self.workspace_documents();
        let reusable = self.diagnostics.lint.results.read().snapshot_for(&file);
        let capture_file = file.clone();
        let (snapshot, document, documents) = {
            let permit = self
                .diagnostics
                .lint
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(std::io::Error::other)?;
            super::state::blocking_result(
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let snapshot = match reusable.filter(|snapshot| snapshot.refreshable(&capture_file))
                    {
                        Some(snapshot) => Arc::new(snapshot.with_overlays(&documents)?),
                        None => {
                            Arc::new(snapshot::ProjectSnapshot::capture(&capture_file, &documents)?)
                        }
                    };
                    Ok::<_, std::io::Error>((snapshot, document, documents))
                })
                .await,
            )?
        };
        if !self.diagnostics_snapshot_current(&uri, &document.snapshot)
            || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Document inputs changed during capture; the next check will re-read them",
            ));
        }
        let revisions = self.lint_revision_fences(&snapshot, &documents);
        let key = PreparationKey {
            root: snapshot.root.clone(),
            config: config.clone(),
            inputs: snapshot.config_digest(),
        };
        self.diagnostics.lint.control.record(
            &uri,
            LintPhase::Preparing,
            "Preparing the analyzer build and its pinned compiler for this project",
        );
        let description = self.ensure_prepared(key, epoch).await?;
        let checked = snapshot.clone();
        let current = super::state::blocking_result(
            tokio::task::spawn_blocking(move || checked.unchanged()).await,
        );
        if !current {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Project inputs changed during preparation; the next check will re-read them",
            ));
        }
        self.diagnostics.lint.control.record(
            &uri,
            LintPhase::Running,
            "Checking the current open project inputs",
        );
        self.client.log_message(MessageType::INFO, format!(
            "bend-lint: executing {} {:?}; external rule/compiler execution was explicitly enabled. {description}. The analyzer's pinned Bend is independent of the selected LSP compiler.", config.path, config.arguments)).await;
        let permit = self
            .diagnostics
            .lint
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(std::io::Error::other)?;
        let output = process::run(
            &config,
            &snapshot.root,
            &file,
            &snapshot.overlays,
            self.compiler.reapers.clone(),
            permit,
        )
        .await?;
        self.finish_lint(document, config, epoch, revisions, snapshot, output)
            .await
    }

    async fn ensure_prepared(
        &self,
        key: PreparationKey,
        epoch: u64,
    ) -> std::io::Result<String> {
        loop {
            let mut receiver = self.diagnostics.lint.preparation_changed.subscribe();
            {
                let mut state = self.diagnostics.lint.preparation.write();
                if let Some(entry) = state.as_ref().filter(|entry| entry.key == key) {
                    match &entry.phase {
                        PreparationPhase::Ready(description) => return Ok(description.clone()),
                        PreparationPhase::Failed(message) => {
                            return Err(std::io::Error::other(message.clone()));
                        }
                        PreparationPhase::Running => {}
                    }
                }
                if !state.as_ref().is_some_and(|entry| entry.key == key) {
                    if let Some(previous) = state.take()
                        && let Some(task) = previous.task
                    {
                        task.abort();
                    }
                    let backend = self.clone();
                    let prepared = key.clone();
                    let task = tokio::spawn(async move {
                        let reporter = backend.clone();
                        super::state::supervise(backend.run_preparation(prepared), move || {
                            reporter.diagnostics.report_fatal()
                        })
                        .await;
                    });
                    *state = Some(Preparation {
                        key: key.clone(),
                        phase: PreparationPhase::Running,
                        task: Some(task),
                    });
                }
            }
            receiver.changed().await.map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "bend-lint preparation stopped with the server",
                )
            })?;
            if self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
                || self.diagnostics.lint.closed.load(Ordering::SeqCst)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "bend-lint preparation superseded by newer changes",
                ));
            }
        }
    }

    async fn run_preparation(&self, key: PreparationKey) {
        let outcome = match self
            .diagnostics
            .lint
            .prep_semaphore
            .clone()
            .acquire_owned()
            .await
        {
            Ok(permit) => {
                process::prepare(
                    &key.config,
                    &key.root,
                    self.compiler.reapers.clone(),
                    permit,
                )
                .await
            }
            Err(error) => Err(std::io::Error::other(error)),
        };
        let mut state = self.diagnostics.lint.preparation.write();
        if let Some(entry) = state.as_mut().filter(|entry| entry.key == key) {
            entry.task = None;
            entry.phase = match outcome {
                Ok(preparation) => PreparationPhase::Ready(preparation.description),
                Err(error) => {
                    PreparationPhase::Failed(format!("bend-lint preparation failed: {error}"))
                }
            };
        }
        drop(state);
        self.diagnostics
            .lint
            .preparation_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    async fn finish_lint(
        &self,
        document: Document,
        config: LintConfig,
        epoch: u64,
        revisions: Vec<(FileId, u64)>,
        snapshot: Arc<snapshot::ProjectSnapshot>,
        output: std::process::Output,
    ) -> std::io::Result<String> {
        let uri = &document.uri;
        if !output.stderr.is_empty() {
            let stderr = &output.stderr[..output.stderr.len().min(16 * 1024)];
            self.client
                .log_message(
                    MessageType::INFO,
                    format!(
                        "bend-lint stderr (up to 16 KiB): {}",
                        String::from_utf8_lossy(stderr)
                    ),
                )
                .await;
        }
        let checked = snapshot.clone();
        let permit = self
            .diagnostics
            .lint
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(std::io::Error::other)?;
        let (findings, unchanged) = super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let findings = wire::parse(&output, &checked)?;
                let unchanged = checked.unchanged();
                Ok::<_, std::io::Error>((findings, unchanged))
            })
            .await,
        )?;
        let partial = findings.skipped.len();
        let notice = findings.notice();
        if let Some(notice) = &notice {
            self.client
                .log_message(MessageType::INFO, notice.clone())
                .await;
        }
        let completion = notice.map_or_else(
            || "Analysis of the current open inputs is complete".to_owned(),
            |notice| {
                format!(
                    "Analysis completed with {partial} skipped check(s); findings from checked inputs are retained: {notice}"
                )
            },
        );
        let _publish = self.diagnostics.imported_publish.lock().await;
        let affected = {
            // Serialize the final epoch check/result store with synchronous
            // invalidation; never hold this state guard across publication.
            let _task = self.diagnostics.lint.task.read();
            if !unchanged
                || !self.lint_revisions_current(&revisions)
                || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
                || !self.diagnostics_snapshot_current(&uri, &document.snapshot)
            {
                None
            } else {
                let affected = self.diagnostics.lint.results.write().insert(
                    uri.clone(),
                    Arc::new(LintResult {
                        epoch,
                        revisions,
                        config,
                        snapshot,
                        findings,
                    }),
                );
                self.diagnostics
                    .lint
                    .control
                    .record(uri, LintPhase::Ready, completion.clone());
                Some(affected)
            }
        };
        let Some(affected) = affected else {
            self.finish_lint_control(&uri, LintPhase::Skipped, "Project inputs changed during analysis; the next check will re-read them", epoch).await;
            return Ok("Analysis skipped; inputs changed during analysis".into());
        };
        self.publish_coordinated_locked(affected).await;
        Ok(completion)
    }
}

async fn join_lint_task(previous: Option<JoinHandle<()>>) {
    if let Some(previous) = previous
        && let Err(error) = previous.await
        && error.is_panic()
    {
        std::panic::resume_unwind(error.into_panic());
    }
}
