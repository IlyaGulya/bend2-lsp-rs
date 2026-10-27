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
    collections::{BTreeSet, HashMap, HashSet},
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

// One coherent open-input read: the immutable document snapshots and their
// revision generations are sampled under the same workspace lock, so no fence
// is ever sampled after the capture it describes. A later change advances the
// stamp and fences the result; publication additionally compares the complete
// current open-input view with the captured one.
struct LintInputs {
    documents: Vec<Document>,
    generations: HashMap<Url, (FileId, u64)>,
}

impl LintInputs {
    fn fences(&self, view: &snapshot::ProjectSnapshot) -> Vec<(FileId, u64)> {
        self.documents
            .iter()
            .filter_map(|document| {
                let path = document.uri.to_file_path().ok()?;
                if !view.covers_path(&path)
                    && !snapshot::input_identity(&path)
                        .is_some_and(|identity| view.covers_path(&identity))
                {
                    return None;
                }
                self.generations.get(&document.uri).copied()
            })
            .collect()
    }
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
        self.current_lint_contributors(uri).into_iter().next()
    }

    // Every current entry that indexes this document, newest first. Status and
    // partial semantics use the first (primary) entry; diagnostics, fixes, and
    // invalidation must consider all of them.
    fn current_lint_contributors(&self, uri: &Url) -> Vec<Arc<LintResult>> {
        let contributors = self.diagnostics.lint.results.read().contributors(uri);
        if contributors.is_empty() {
            return contributors;
        }
        let documents = self.workspace_documents();
        contributors
            .into_iter()
            .filter(|result| self.lint_result_current_in(result, &documents))
            .collect()
    }

    // Publication and retained-result use share this fence: the immutable
    // captured view must still be the current complete open-input view, not
    // only the root document or the generations sampled with the capture.
    fn lint_result_current(&self, result: &LintResult) -> bool {
        self.lint_result_current_in(result, &self.workspace_documents())
    }

    fn lint_result_current_in(&self, result: &LintResult, documents: &[Document]) -> bool {
        *self.diagnostics.lint.config.read() == result.config
            && self.lint_revisions_current(&result.revisions)
            && result.snapshot.open_documents_current(documents)
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

    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "open_inputs", result_count = tracing::field::Empty))]
    fn lint_inputs(&self) -> LintInputs {
        let state = self.workspace.state.read();
        let documents = state.database.open_documents();
        tracing::Span::current().record("result_count", documents.len());
        let generations = documents
            .iter()
            .filter_map(|document| {
                let file = state.database.file_id_by_uri(&document.uri)?;
                let sync = state.revisions.get(&file)?;
                Some((
                    document.uri.clone(),
                    (file, revision_result(sync.status()).generation()),
                ))
            })
            .collect();
        LintInputs {
            documents,
            generations,
        }
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
        let mut combined = Vec::new();
        for result in self.current_lint_contributors(uri) {
            let Some(items) = result.findings.by_uri.get(uri) else {
                continue;
            };
            for item in items {
                if !combined.contains(item) {
                    combined.push(item.clone());
                }
            }
        }
        combined
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
        let retired_progress = {
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
            // Retire the superseded run's status and progress before its
            // replacement exists: the replacement cannot own a token yet, so
            // only work that is already finished can be ended here.
            self.diagnostics
                .lint
                .control
                .invalidate_running("Superseded by newer editor changes");
            let retiring = self.take_progress();
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
                super::state::supervise(operation, move || reporter.diagnostics.report_fatal())
                    .await;
            });
            *task = Some(handle);
            retiring
        };
        self.end_lint_progress_tokens(retired_progress, "Superseded by newer editor changes")
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
                self.finish_lint_control(&uri, phase, &message, epoch).await;
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
        let reusable = self.diagnostics.lint.results.read().snapshot_for(&file);
        let inputs = self.lint_inputs();
        let capture_file = file.clone();
        let documents = inputs.documents.clone();
        let (snapshot, document) = {
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
                    let snapshot =
                        match reusable.filter(|snapshot| snapshot.refreshable(&capture_file)) {
                            Some(snapshot) => Arc::new(snapshot.with_overlays(&documents)?),
                            None => Arc::new(snapshot::ProjectSnapshot::capture(
                                &capture_file,
                                &documents,
                            )?),
                        };
                    Ok::<_, std::io::Error>((snapshot, document))
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
        // Fences are the generations sampled with the immutable capture, never
        // a generation observed after an asynchronous capture completed.
        let revisions = inputs.fences(&snapshot);
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

    async fn ensure_prepared(&self, key: PreparationKey, epoch: u64) -> std::io::Result<String> {
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
        let result = LintResult {
            epoch,
            revisions,
            config,
            snapshot,
            findings,
        };
        let affected = {
            // Serialize the final epoch check/result store with synchronous
            // invalidation; never hold this state guard across publication.
            let _task = self.diagnostics.lint.task.read();
            if !unchanged
                || self.diagnostics.lint.epoch.load(Ordering::SeqCst) != epoch
                || !self.diagnostics_snapshot_current(&uri, &document.snapshot)
                || !self.lint_result_current(&result)
            {
                None
            } else {
                let affected = self
                    .diagnostics
                    .lint
                    .results
                    .write()
                    .insert(uri.clone(), Arc::new(result));
                self.diagnostics
                    .lint
                    .control
                    .record(uri, LintPhase::Ready, completion.clone());
                Some(affected)
            }
        };
        let Some(affected) = affected else {
            self.finish_lint_control(
                &uri,
                LintPhase::Skipped,
                "Project inputs changed during analysis; the next check will re-read them",
                epoch,
            )
            .await;
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

#[cfg(test)]
mod tests {
    use super::{
        Backend, DANGEROUS_KIND, FileId, LintConfig, LintResult, revision_result, snapshot, wire,
    };
    use crate::server::{
        compiler::CompilerReapers, diagnostics::DiagnosticsService,
        workspace_service::WorkspaceService,
    };
    use std::{
        collections::HashMap,
        error::Error,
        fs, io,
        path::PathBuf,
        sync::{Arc, atomic::Ordering},
    };
    use tokio::sync::Semaphore;
    use tower_lsp::{
        LspService,
        lsp_types::{
            CodeAction, CodeActionOrCommand, CodeActionParams, Diagnostic, DiagnosticSeverity,
            DocumentChanges, OneOf, Position, Range,
        },
    };
    use url::Url;

    const MAIN: &str = "import ./dep.bend as Dep\ndef main = Dep.value\n";
    const DEP: &str = "def value = 1\n";
    const CHANGED_DEP: &str = "def value = 2\n";
    const NEW_INPUT: &str = "def extra = 2\n";

    struct Project {
        _directory: tempfile::TempDir,
        main: PathBuf,
        dep: PathBuf,
        main_uri: Url,
        dep_uri: Url,
    }

    fn project() -> Result<Project, Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let main = directory.path().join("main.bend");
        let dep = directory.path().join("dep.bend");
        fs::write(&main, MAIN)?;
        fs::write(&dep, DEP)?;
        fs::write(directory.path().join("bend-lint.json"), "{\"rules\":{}}")?;
        let main_uri =
            Url::from_file_path(&main).map_err(|()| io::Error::other("main file URI"))?;
        let dep_uri =
            Url::from_file_path(&dep).map_err(|()| io::Error::other("dependency file URI"))?;
        Ok(Project {
            _directory: directory,
            main,
            dep,
            main_uri,
            dep_uri,
        })
    }

    // A retained result only exists after the provider is opted in with a
    // configured executable. The test binary is that absolute executable; no
    // check is scheduled here, so it is never invoked.
    fn lint_config() -> Result<LintConfig, Box<dyn Error>> {
        let path = std::env::current_exe()?
            .into_os_string()
            .into_string()
            .map_err(|_| io::Error::other("the test executable path is not UTF-8"))?;
        Ok(LintConfig {
            enabled: true,
            dangerous_code_actions: true,
            path,
            ..LintConfig::default()
        })
    }

    fn backend() -> Result<Backend, Box<dyn Error>> {
        let workspace = Arc::new(WorkspaceService {
            staging: Arc::new(Semaphore::new(1)),
            ..WorkspaceService::default()
        });
        let (service, _socket) = LspService::new(|client| {
            Backend::new(
                client,
                workspace,
                Arc::new(DiagnosticsService::default()),
                Arc::new(CompilerReapers::default()),
            )
        });
        let backend = service.inner().clone();
        *backend.diagnostics.lint.config.write() = lint_config()?;
        Ok(backend)
    }

    async fn open(
        backend: &Backend,
        uri: &Url,
        version: i32,
        text: &str,
        path: PathBuf,
    ) -> Result<(), Box<dyn Error>> {
        backend
            .open_workspace_text(uri.clone(), "bend".into(), version, text.into(), Some(path))
            .await
            .ok_or_else(|| io::Error::other("the open document should commit"))?;
        Ok(())
    }

    fn fence(backend: &Backend, uri: &Url) -> Result<(FileId, u64), Box<dyn Error>> {
        let state = backend.workspace.state.read();
        let file = state
            .database
            .file_id_by_uri(uri)
            .ok_or_else(|| io::Error::other("open input identity"))?;
        let sync = state
            .revisions
            .get(&file)
            .ok_or_else(|| io::Error::other("open input revision"))?;
        Ok((file, revision_result(sync.status()).generation()))
    }

    fn retain(
        backend: &Backend,
        uri: &Url,
        snapshot: Arc<snapshot::ProjectSnapshot>,
        revisions: Vec<(FileId, u64)>,
    ) {
        let epoch = backend.diagnostics.lint.epoch.load(Ordering::SeqCst);
        retain_entry(
            backend,
            uri,
            entry(backend, epoch, &snapshot, revisions, no_findings()),
        );
    }

    fn diagnostic(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            severity: Some(DiagnosticSeverity::WARNING),
            source: Some("bend-lint".into()),
            message: message.into(),
            ..Diagnostic::default()
        }
    }

    fn no_findings() -> wire::Findings {
        wire::Findings {
            by_uri: HashMap::new(),
            actionable: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn fixture(
        backend: &Backend,
        project: &Project,
    ) -> Result<Arc<snapshot::ProjectSnapshot>, Box<dyn Error>> {
        let captured = backend.workspace_documents();
        Ok(Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &captured,
        )?))
    }

    fn entry(
        backend: &Backend,
        epoch: u64,
        snapshot: &Arc<snapshot::ProjectSnapshot>,
        revisions: Vec<(FileId, u64)>,
        findings: wire::Findings,
    ) -> Arc<LintResult> {
        Arc::new(LintResult {
            epoch,
            revisions,
            config: backend.diagnostics.lint.config.read().clone(),
            snapshot: snapshot.clone(),
            findings,
        })
    }

    fn retain_entry(backend: &Backend, uri: &Url, result: Arc<LintResult>) {
        backend
            .diagnostics
            .lint
            .results
            .write()
            .insert(uri.clone(), result);
    }

    fn actionable(message: &str, uri: &Url) -> wire::Findings {
        let reported = diagnostic(message);
        let mut findings = no_findings();
        findings.by_uri.insert(uri.clone(), vec![reported.clone()]);
        findings.actionable.push(wire::ActionableFinding {
            uri: uri.clone(),
            diagnostic: reported,
            fixes: vec![wire::Fix {
                title: format!("Apply {message}"),
                applicability: "dangerous".into(),
                edits: vec![wire::Edit {
                    path: "main.bend".into(),
                    range: Range::new(Position::new(0, 0), Position::new(0, 1)),
                    text: "def identity: U32".into(),
                }],
            }],
        });
        findings
    }

    fn dangerous_params(uri: &Url) -> Result<CodeActionParams, Box<dyn Error>> {
        Ok(serde_json::from_value(serde_json::json!({
            "textDocument": {"uri": uri},
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "context": {"diagnostics": [], "only": [DANGEROUS_KIND], "triggerKind": 1}
        }))?)
    }

    fn dangerous_actions(actions: &[CodeActionOrCommand]) -> Vec<&CodeAction> {
        actions
            .iter()
            .filter_map(|action| match action {
                CodeActionOrCommand::CodeAction(action)
                    if action
                        .kind
                        .as_ref()
                        .is_some_and(|kind| kind.as_str() == DANGEROUS_KIND) =>
                {
                    Some(action)
                }
                _ => None,
            })
            .collect()
    }

    // A dependency changes after the immutable capture but before publication.
    // Sampling generations after the capture pairs older text with the newer
    // generation, so the retained entry must still be rejected.
    #[tokio::test]
    async fn captured_text_cannot_be_retained_under_a_newer_dependency_generation()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let captured = backend.workspace_documents();
        let snapshot = Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &captured,
        )?);
        open(
            &backend,
            &project.dep_uri,
            2,
            CHANGED_DEP,
            project.dep.clone(),
        )
        .await?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        retain(&backend, &project.main_uri, snapshot, revisions);
        assert!(
            backend.current_lint_result(&project.main_uri).is_none(),
            "captured dependency text outlived the newer generation it was paired with"
        );
        Ok(())
    }

    // A covered input opens after the capture and is absent from the captured
    // fence list entirely. Its unsaved text was never analyzed, so the retained
    // entry is not current.
    #[tokio::test]
    async fn newly_opened_covered_input_invalidates_a_retained_result() -> Result<(), Box<dyn Error>>
    {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        let captured = backend.workspace_documents();
        let snapshot = Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &captured,
        )?);
        let revisions = vec![fence(&backend, &project.main_uri)?];
        retain(&backend, &project.main_uri, snapshot, revisions);
        let extra = project
            .main
            .parent()
            .ok_or_else(|| io::Error::other("project root"))?
            .join("extra.bend");
        let extra_uri =
            Url::from_file_path(&extra).map_err(|()| io::Error::other("new input URI"))?;
        open(&backend, &extra_uri, 1, NEW_INPUT, extra).await?;
        assert!(
            backend.current_lint_result(&project.main_uri).is_none(),
            "a covered input opened after the capture left the older result current"
        );
        Ok(())
    }

    // Fences sampled together with the immutable capture cannot describe a
    // generation that only exists after those snapshots were taken.
    #[tokio::test]
    async fn capture_time_fences_reject_a_dependency_changed_after_sampling()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let inputs = backend.lint_inputs();
        let snapshot = Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &inputs.documents,
        )?);
        let revisions = inputs.fences(&snapshot);
        assert!(revisions.contains(&fence(&backend, &project.dep_uri)?));
        open(
            &backend,
            &project.dep_uri,
            2,
            CHANGED_DEP,
            project.dep.clone(),
        )
        .await?;
        assert!(
            !backend.lint_revisions_current(&revisions),
            "fences sampled with the capture described a later generation"
        );
        Ok(())
    }

    // Currentness is scoped to the captured project view: other projects and
    // refreshed unrelated inputs must not retire a valid result.
    #[tokio::test]
    async fn unrelated_open_inputs_keep_a_retained_result_current() -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        let captured = backend.workspace_documents();
        let snapshot = Arc::new(snapshot::ProjectSnapshot::capture(
            &project.main,
            &captured,
        )?);
        let revisions = vec![fence(&backend, &project.main_uri)?];
        retain(&backend, &project.main_uri, snapshot, revisions);
        let other = tempfile::tempdir()?;
        let other_path = other.path().join("other.bend");
        fs::write(&other_path, NEW_INPUT)?;
        let other_uri =
            Url::from_file_path(&other_path).map_err(|()| io::Error::other("other file URI"))?;
        open(&backend, &other_uri, 1, NEW_INPUT, other_path.clone()).await?;
        open(&backend, &other_uri, 2, NEW_INPUT, other_path).await?;
        assert!(
            backend.current_lint_result(&project.main_uri).is_some(),
            "an unrelated project input retired a still-current result"
        );
        Ok(())
    }

    // Every current entry that indexes the target contributes. An empty own
    // report at a newer epoch must not erase the sibling entry's import.
    #[tokio::test]
    async fn an_older_imported_contribution_survives_a_newer_empty_own_result()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        let imported = diagnostic("imported rule");
        let mut imported_findings = no_findings();
        imported_findings
            .by_uri
            .insert(project.main_uri.clone(), vec![imported.clone()]);
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(&backend, 1, &snapshot, revisions.clone(), imported_findings),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(&backend, 2, &snapshot, revisions, no_findings()),
        );
        assert_eq!(
            backend.lint_diagnostics(&project.main_uri),
            vec![imported],
            "a newer empty own report must not erase a current imported contribution"
        );
        assert!(
            backend
                .current_lint_result(&project.main_uri)
                .is_some_and(|result| result.epoch == 2),
            "status must keep the newest own entry as the primary result"
        );
        Ok(())
    }

    // Distinct contributions from two current entries are published together,
    // and an identical diagnostic reported by both appears once.
    #[tokio::test]
    async fn distinct_contributions_from_two_entries_union_and_dedup() -> Result<(), Box<dyn Error>>
    {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        let own = diagnostic("own rule");
        let imported = diagnostic("imported rule");
        let shared = diagnostic("shared rule");
        let mut own_findings = no_findings();
        own_findings
            .by_uri
            .insert(project.main_uri.clone(), vec![own.clone(), shared.clone()]);
        let mut imported_findings = no_findings();
        imported_findings.by_uri.insert(
            project.main_uri.clone(),
            vec![imported.clone(), shared.clone()],
        );
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(&backend, 1, &snapshot, revisions.clone(), imported_findings),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(&backend, 2, &snapshot, revisions, own_findings),
        );
        let diagnostics = backend.lint_diagnostics(&project.main_uri);
        assert_eq!(
            diagnostics.len(),
            3,
            "both current contributors must be published exactly once"
        );
        for expected in [&own, &imported, &shared] {
            assert!(
                diagnostics.contains(expected),
                "missing contributor diagnostic"
            );
        }
        assert_eq!(
            diagnostics.iter().filter(|item| *item == &shared).count(),
            1,
            "an identical diagnostic must be deduplicated"
        );
        Ok(())
    }

    // Replacing one contributor with a clean report clears only that entry's
    // contribution; the other entry's findings stay published.
    #[tokio::test]
    async fn replacing_a_contributor_with_an_empty_report_clears_only_its_findings()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        let own = diagnostic("own rule");
        let imported = diagnostic("imported rule");
        let mut own_findings = no_findings();
        own_findings
            .by_uri
            .insert(project.main_uri.clone(), vec![own.clone()]);
        let mut imported_findings = no_findings();
        imported_findings
            .by_uri
            .insert(project.main_uri.clone(), vec![imported.clone()]);
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(&backend, 1, &snapshot, revisions.clone(), imported_findings),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(&backend, 2, &snapshot, revisions.clone(), own_findings),
        );
        let initial = backend.lint_diagnostics(&project.main_uri);
        assert!(initial.contains(&own) && initial.contains(&imported));
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(&backend, 3, &snapshot, revisions, no_findings()),
        );
        assert_eq!(
            backend.lint_diagnostics(&project.main_uri),
            vec![own],
            "only the replaced contributor's finding may clear"
        );
        Ok(())
    }

    // The imported finding's fix stays available, versioned to the analyzed
    // document, even while the newer own entry reports nothing.
    #[tokio::test]
    async fn an_imported_dangerous_fix_survives_a_newer_empty_own_result()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(
                &backend,
                1,
                &snapshot,
                revisions.clone(),
                actionable("imported rule", &project.main_uri),
            ),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(&backend, 2, &snapshot, revisions, no_findings()),
        );
        let params = dangerous_params(&project.main_uri)?;
        let actions = backend.lint_code_actions(&params).await;
        let dangerous = dangerous_actions(&actions);
        let action = dangerous
            .first()
            .copied()
            .ok_or_else(|| io::Error::other("the imported dangerous fix must stay available"))?;
        assert_eq!(
            action.diagnostics.as_ref(),
            Some(&vec![diagnostic("imported rule")]),
            "the action must keep the importing authority's diagnostic"
        );
        let changes = action
            .edit
            .as_ref()
            .and_then(|edit| edit.document_changes.as_ref())
            .ok_or_else(|| io::Error::other("the dangerous fix must carry edits"))?;
        let DocumentChanges::Edits(edits) = changes else {
            return Err(io::Error::other("the dangerous fix must use text edits").into());
        };
        let targeted = edits
            .iter()
            .find(|edit| edit.text_document.uri == project.main_uri)
            .ok_or_else(|| io::Error::other("the imported fix must target the root document"))?;
        assert_eq!(
            targeted.text_document.version,
            Some(1),
            "the fix must stay versioned to the analyzed document"
        );
        assert_eq!(targeted.edits.len(), 1);
        assert!(
            matches!(&targeted.edits[0], OneOf::Left(edit) if edit.new_text == "def identity: U32")
        );
        Ok(())
    }

    // The same fix reported by two current entries is offered once.
    #[tokio::test]
    async fn identical_imported_fixes_from_two_entries_are_deduplicated()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        retain_entry(
            &backend,
            &project.dep_uri,
            entry(
                &backend,
                1,
                &snapshot,
                revisions.clone(),
                actionable("shared rule", &project.main_uri),
            ),
        );
        retain_entry(
            &backend,
            &project.main_uri,
            entry(
                &backend,
                2,
                &snapshot,
                revisions,
                actionable("shared rule", &project.main_uri),
            ),
        );
        let params = dangerous_params(&project.main_uri)?;
        let actions = backend.lint_code_actions(&params).await;
        assert_eq!(
            dangerous_actions(&actions).len(),
            1,
            "identical fixes from two current entries must be deduplicated"
        );
        Ok(())
    }
}
