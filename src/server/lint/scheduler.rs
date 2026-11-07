use super::{
    CANCELLED_REASON, LintConfig, LintResult,
    control::{LintControl, LintPhase},
    process,
    project::{
        Preparation, PreparationKey, PreparationPhase, PreparationToken, ProjectKey, ProjectState,
    },
    results::LintResults,
    snapshot, wire,
};
use crate::{
    analysis::DocumentSnapshot,
    server::{
        lsp::Backend,
        observability::{
            buffers::diagnostic_backing_bytes,
            work::{CancelOnDrop, WorkKind},
        },
        state::{State, blocking_result, supervise},
    },
    workspace::Document,
};
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
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

const FROZEN_REASON: &str = "Project inputs changed without an accepted event; the current revision's view is frozen and the next check will re-read them";

// Bounded lint execution state: one analyzer lease and one preparation lease
// for the whole server, plus per-project queues, revisions and preparation
// ownership. The project key itself lives in `project`.
pub(in crate::server) struct LintService {
    pub(super) config: State<LintConfig>,
    pub(super) results: State<LintResults>,
    pub(super) control: LintControl,
    pub(super) projects: State<HashMap<ProjectKey, ProjectState>>,
    pub(super) preparation_changed: watch::Sender<u64>,
    pub(super) prep_semaphore: Arc<Semaphore>,
    pub(super) epoch: AtomicU64,
    pub(super) progress: AtomicU64,
    pub(super) next_task: AtomicU64,
    pub(super) preparations: AtomicU64,
    pub(super) closed: AtomicBool,
    pub(super) semaphore: Arc<Semaphore>,
}

struct LintCapture {
    document: Document,
    revisions: Vec<(Url, u64)>,
    snapshot: Arc<snapshot::ProjectSnapshot>,
}

impl Default for LintService {
    fn default() -> Self {
        let (preparation_changed, _) = watch::channel(0u64);
        Self {
            config: State::new(LintConfig::default()),
            results: State::new(LintResults::default()),
            control: LintControl::default(),
            projects: State::new(HashMap::new()),
            preparation_changed,
            prep_semaphore: Arc::new(Semaphore::new(1)),
            epoch: AtomicU64::new(0),
            progress: AtomicU64::new(0),
            next_task: AtomicU64::new(0),
            preparations: AtomicU64::new(0),
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

    pub(super) fn next_progress(&self) -> u64 {
        self.progress.fetch_add(1, Ordering::SeqCst)
    }

    fn next_task(&self) -> u64 {
        self.next_task.fetch_add(1, Ordering::SeqCst)
    }

    fn next_preparation(&self) -> PreparationToken {
        PreparationToken::new(self.preparations.fetch_add(1, Ordering::SeqCst))
    }

    fn take_results(&self) -> HashSet<Url> {
        self.results.write().clear()
    }

    pub(super) fn project_revision(&self, key: &ProjectKey) -> Option<u64> {
        self.projects.read().get(key).map(|state| state.revision)
    }

    /// Retained lint-owned read-set accounting. Every byte figure counts each
    /// unique `DocumentSnapshot` `Arc` once across all retained project views
    /// and results: `source_bytes` is the logical text length and
    /// `source_capacity_bytes` the allocated text buffer. `overlay_bytes` and
    /// `trusted_rule_bytes` are labeled subsets already included in
    /// `source_bytes` when the capture shares those `Arc`s; they are never
    /// added twice. Line-index and syntax buffers are not accessible from this
    /// module and are intentionally omitted rather than estimated.
    pub(in crate::server) fn diagnostic_usage(&self) -> LintDiagnosticUsage {
        let mut usage = LintDiagnosticUsage::default();
        let mut seen_views = HashSet::new();
        let mut seen_sources = HashSet::new();
        let mut seen_overlays = HashSet::new();
        let mut seen_rules = HashSet::new();
        {
            let results = self.results.read();
            usage.retained_results = results.retained_count();
            usage.retained_contributors = results.contributor_count();
            for result in results.retained() {
                for diagnostic in result.findings.by_uri.values().flatten() {
                    usage.retained_diagnostic_backing_bytes +=
                        u64::try_from(diagnostic_backing_bytes(diagnostic)).unwrap_or(u64::MAX);
                }
                account_project_view(
                    &result.snapshot,
                    &mut seen_views,
                    &mut seen_sources,
                    &mut seen_overlays,
                    &mut seen_rules,
                    &mut usage,
                );
            }
        }
        {
            let projects = self.projects.read();
            usage.project_states = projects.len();
            for state in projects.values() {
                if let Some((_, view)) = &state.view {
                    usage.project_views += 1;
                    account_project_view(
                        view,
                        &mut seen_views,
                        &mut seen_sources,
                        &mut seen_overlays,
                        &mut seen_rules,
                        &mut usage,
                    );
                }
                if state.preparation.is_some() {
                    usage.preparations += 1;
                }
            }
        }
        usage
    }
}

fn account_project_view(
    view: &Arc<snapshot::ProjectSnapshot>,
    seen_views: &mut HashSet<*const snapshot::ProjectSnapshot>,
    seen_sources: &mut HashSet<*const DocumentSnapshot>,
    seen_overlays: &mut HashSet<*const DocumentSnapshot>,
    seen_rules: &mut HashSet<*const DocumentSnapshot>,
    usage: &mut LintDiagnosticUsage,
) {
    if !seen_views.insert(Arc::as_ptr(view)) {
        return;
    }
    usage.project_snapshots += 1;
    for snapshot in view.source_snapshots() {
        if seen_sources.insert(Arc::as_ptr(snapshot)) {
            usage.source_snapshots += 1;
            usage.source_bytes += u64::try_from(snapshot.text.len()).unwrap_or(u64::MAX);
            usage.source_capacity_bytes +=
                u64::try_from(snapshot.text.capacity()).unwrap_or(u64::MAX);
        }
    }
    for snapshot in view.overlay_snapshots() {
        if seen_overlays.insert(Arc::as_ptr(snapshot)) {
            usage.overlay_snapshots += 1;
            usage.overlay_bytes += u64::try_from(snapshot.text.len()).unwrap_or(u64::MAX);
        }
    }
    for snapshot in view.trusted_rule_snapshots() {
        if seen_rules.insert(Arc::as_ptr(snapshot)) {
            usage.trusted_rule_snapshots += 1;
            usage.trusted_rule_bytes += u64::try_from(snapshot.text.len()).unwrap_or(u64::MAX);
        }
    }
}

/// Retained lint-owned buffers, exposed for the local diagnostics report.
/// Counts are entries; byte fields are logical text or allocated text buffers
/// as named. No paths, rule text or diagnostic payloads are retained here.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::server) struct LintDiagnosticUsage {
    pub(in crate::server) project_snapshots: usize,
    pub(in crate::server) source_snapshots: usize,
    pub(in crate::server) source_bytes: u64,
    pub(in crate::server) source_capacity_bytes: u64,
    pub(in crate::server) overlay_snapshots: usize,
    pub(in crate::server) overlay_bytes: u64,
    pub(in crate::server) trusted_rule_snapshots: usize,
    pub(in crate::server) trusted_rule_bytes: u64,
    pub(in crate::server) retained_results: usize,
    pub(in crate::server) retained_contributors: usize,
    pub(in crate::server) retained_diagnostic_backing_bytes: u64,
    pub(in crate::server) project_views: usize,
    pub(in crate::server) preparations: usize,
    pub(in crate::server) project_states: usize,
}

// Test-only ordering gate on the preparation waiting path. A regression arms it
// for exactly one project, so its waiting check observes a worker that has
// already published its completion before the waiter re-reads the attempt. It
// never captures an unrelated preparation, and the bounded wait keeps an
// accidentally armed gate from hanging another test.
#[cfg(test)]
pub(super) static PREPARATION_GATE: PreparationRegistrationGate =
    PreparationRegistrationGate::new();

#[cfg(test)]
pub(super) struct PreparationRegistrationGate {
    armed: std::sync::Mutex<Option<ProjectKey>>,
    release: tokio::sync::Notify,
}

#[cfg(test)]
impl PreparationRegistrationGate {
    const fn new() -> Self {
        Self {
            armed: std::sync::Mutex::new(None),
            release: tokio::sync::Notify::const_new(),
        }
    }

    pub(super) fn arm(&self, project: &ProjectKey) {
        *self.lock() = Some(project.clone());
    }

    pub(super) fn disarm(&self) {
        self.lock().take();
        self.release.notify_waiters();
    }

    async fn hold(&self, project: &ProjectKey) {
        let armed = self.lock().as_ref().is_some_and(|target| target == project);
        if !armed {
            return;
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.release.notified()).await;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<ProjectKey>> {
        self.armed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(super) async fn join_lint_task(previous: Option<JoinHandle<()>>) {
    if let Some(previous) = previous
        && let Err(error) = previous.await
        && error.is_panic()
    {
        std::panic::resume_unwind(error.into_panic());
    }
}

impl Backend {
    pub(in crate::server) async fn shutdown_lint(&self) {
        let lint = &self.diagnostics.lint;
        lint.closed.store(true, Ordering::SeqCst);
        lint.semaphore.close();
        lint.prep_semaphore.close();
        let mut handles = Vec::new();
        {
            let mut projects = lint.projects.write();
            for state in projects.values_mut() {
                if let Some((_, task)) = state.task.take() {
                    task.abort();
                    handles.push(task);
                }
                if let Some(preparation) = state.preparation.take()
                    && let Some(task) = preparation.task
                {
                    task.abort();
                    handles.push(task);
                }
            }
            projects.clear();
        }
        lint.preparation_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        for handle in handles {
            join_lint_task(Some(handle)).await;
        }
        self.end_all_lint_progress(CANCELLED_REASON).await;
        let affected = self.diagnostics.lint.take_results();
        if !affected.is_empty() {
            self.publish_coordinated_diagnostics(affected).await;
        }
    }

    pub(super) fn enqueue_project(&self, key: ProjectKey, entries: BTreeSet<Url>, debounce: bool) {
        let task = self.diagnostics.lint.next_task();
        let backend = self.clone();
        let spawned_key = key.clone();
        let mut projects = self.diagnostics.lint.projects.write();
        let state = projects.entry(key).or_default();
        // One attempt per entry per revision: a late plan for an entry that is
        // already running or finished in this revision must not run it again.
        let fresh: BTreeSet<Url> = entries
            .into_iter()
            .filter(|uri| state.active.as_ref() != Some(uri) && !state.completed.contains(uri))
            .collect();
        state.pending.extend(fresh);
        if state.task.is_some() || state.pending.is_empty() {
            // A newer scheduler already owns this project; it drains the queue.
            return;
        }
        let revision = state.revision;
        let queued = self.workspace.observability.work.enqueue(WorkKind::Lint);
        let handle = tokio::spawn(async move {
            let reporter = backend.clone();
            let operation = async move {
                if debounce {
                    tokio::time::sleep(LINT_DEBOUNCE).await;
                }
                let mut running = CancelOnDrop::new(queued.start());
                backend.run_project(spawned_key, revision, task).await;
                running.disarm();
            };
            supervise(operation, move || reporter.diagnostics.report_fatal()).await;
        });
        state.task = Some((task, handle));
    }

    async fn run_project(&self, key: ProjectKey, revision: u64, task: u64) {
        loop {
            if self.diagnostics.lint.closed.load(Ordering::SeqCst) {
                return;
            }
            let uri = {
                let mut projects = self.diagnostics.lint.projects.write();
                let Some(state) = projects.get_mut(&key) else {
                    return;
                };
                if state.revision != revision {
                    // A newer owner already handed these entries to its own plan.
                    return;
                }
                let Some(uri) = state.pending.pop_first() else {
                    if state
                        .task
                        .as_ref()
                        .is_some_and(|(current, _)| *current == task)
                    {
                        state.task = None;
                    }
                    state.active = None;
                    return;
                };
                state.active = Some(uri.clone());
                uri
            };
            if let Some(document) = self.lint_open_document(&uri).filter(Self::supported) {
                let config = self.diagnostics.lint.config.read().clone();
                self.run_lint(document, config, &key, revision).await;
            }
            let mut projects = self.diagnostics.lint.projects.write();
            if let Some(state) = projects.get_mut(&key) {
                if state.revision == revision {
                    state.completed.insert(uri.clone());
                }
                if state.active.as_ref().is_some_and(|active| *active == uri) {
                    state.active = None;
                }
            }
        }
    }

    async fn run_lint(
        &self,
        document: Document,
        config: LintConfig,
        project: &ProjectKey,
        revision: u64,
    ) {
        let uri = document.uri.clone();
        let progress = self.begin_lint_progress(&uri, project, revision).await;
        let reason = match self.execute_lint(document, config, project, revision).await {
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
                self.finish_lint_control(&uri, phase, &message, project, revision)
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
        project: &ProjectKey,
        revision: u64,
    ) -> std::io::Result<String> {
        let uri = document.uri.clone();
        let Some(file) = uri.to_file_path().ok() else {
            self.finish_lint_control(
                &uri,
                LintPhase::Skipped,
                "Save this document to a file before checking",
                project,
                revision,
            )
            .await;
            return Ok("Analysis skipped; the document has no file path".into());
        };
        let capture = self
            .capture_lint_view(document, &file, project, revision)
            .await?;
        let snapshot = &capture.snapshot;
        let key = PreparationKey {
            root: snapshot.root.clone(),
            config: config.clone(),
            inputs: snapshot.config_digest(),
        };
        self.diagnostics.lint.control.record(
            &uri,
            LintPhase::Preparing,
            "Preparing the analyzer build and its pinned compiler for this project",
            revision,
        );
        let description = self.ensure_prepared(project, revision, key).await?;
        let checked = Arc::clone(snapshot);
        let current =
            blocking_result(tokio::task::spawn_blocking(move || checked.unchanged()).await);
        if !current || self.diagnostics.lint.project_revision(project) != Some(revision) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Project inputs changed during preparation; the next check will re-read them",
            ));
        }
        self.diagnostics.lint.control.record(
            &uri,
            LintPhase::Running,
            "Checking the current open project inputs",
            revision,
        );
        self.client.log_message(MessageType::INFO, format!(
            "bend-lint: executing {} {:?}; external rule/compiler execution was explicitly enabled. {description}. The analyzer's pinned Bend is independent of the selected LSP compiler.", config.path, config.arguments)).await;
        // A virtual entry below a missing directory must use the captured
        // identity, just like its overlay key, not the editor's filesystem alias.
        let entry = snapshot.entry_path(&file)?;
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
            entry,
            &snapshot.overlays,
            self.compiler.reapers.clone(),
            permit,
        )
        .await?;
        self.finish_lint(capture, config, project, revision, output)
            .await
    }

    async fn capture_lint_view(
        &self,
        document: Document,
        file: &Path,
        project: &ProjectKey,
        revision: u64,
    ) -> std::io::Result<LintCapture> {
        // A revision never replaces its frozen view with newly read inputs.
        let frozen = {
            let projects = self.diagnostics.lint.projects.read();
            projects
                .get(project)
                .and_then(|state| state.view.as_ref())
                .filter(|(view_revision, _)| *view_revision == revision)
                .map(|(_, view)| view.clone())
        };
        let reusable = if frozen.is_none() {
            self.diagnostics.lint.results.read().snapshot_for(file)
        } else {
            None
        };
        let inputs = self.lint_inputs();
        let capture_file = file.to_path_buf();
        let documents = inputs.documents.clone();
        let observation = self.workspace.observability.clone();
        let (snapshot, document) = {
            let permit = self
                .diagnostics
                .lint
                .semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(std::io::Error::other)?;
            blocking_result(
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let snapshot = if let Some(view) = frozen {
                        if !view.refreshable(&capture_file)
                            || !view.open_documents_current(&documents)
                        {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::WouldBlock,
                                FROZEN_REASON,
                            ));
                        }
                        view
                    } else {
                        let snapshot = match reusable.filter(|snapshot| snapshot.refreshable(&capture_file)) {
                            Some(snapshot) => Arc::new(snapshot.with_overlays(&documents)?),
                            None => Arc::new(snapshot::ProjectSnapshot::capture(
                                &capture_file,
                                &documents,
                            )?),
                        };
                        for source in snapshot.source_snapshots()
                            .chain(snapshot.overlay_snapshots())
                            .chain(snapshot.trusted_rule_snapshots())
                        {
                            observation.storage.track_snapshot(source);
                        }
                        snapshot
                    };
                    Ok::<_, std::io::Error>((snapshot, document))
                })
                .await,
            )?
        };
        if !self.diagnostics_snapshot_current(&document.uri, &document.snapshot)
            || self.diagnostics.lint.project_revision(project) != Some(revision)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "Document inputs changed during capture; the next check will re-read them",
            ));
        }
        {
            let mut projects = self.diagnostics.lint.projects.write();
            if let Some(state) = projects.get_mut(project)
                && state.revision == revision
            {
                state.view = Some((revision, snapshot.clone()));
            }
        }
        // Generations belong to the coherent read sampled before capture.
        Ok(LintCapture {
            revisions: super::lint_fences(&inputs, &snapshot),
            document,
            snapshot,
        })
    }

    pub(super) async fn ensure_prepared(
        &self,
        project: &ProjectKey,
        revision: u64,
        key: PreparationKey,
    ) -> std::io::Result<String> {
        loop {
            let mut receiver = self.diagnostics.lint.preparation_changed.subscribe();
            {
                // One critical section decides and registers: the attempt with
                // its completion token exists before the worker it spawns can
                // publish, so an early completion is never dropped and an older
                // same-key completion can never mutate its replacement.
                // `tokio::spawn` does not await, so this guard is never held
                // across an await point.
                let mut projects = self.diagnostics.lint.projects.write();
                let Some(state) = projects.get_mut(project) else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "bend-lint project closed while preparing",
                    ));
                };
                if state.revision != revision {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "bend-lint preparation superseded by newer changes",
                    ));
                }
                let start = match state
                    .preparation
                    .as_ref()
                    .filter(|entry| entry.key == key)
                    .map(|entry| &entry.phase)
                {
                    Some(PreparationPhase::Ready(description)) => {
                        return Ok(description.clone());
                    }
                    Some(PreparationPhase::Failed(message)) => {
                        return Err(std::io::Error::other(message.clone()));
                    }
                    Some(PreparationPhase::Running) => false,
                    None => true,
                };
                if start {
                    let token = self.diagnostics.lint.next_preparation();
                    let backend = self.clone();
                    let prepared = key.clone();
                    let spawned_project = project.clone();
                    let task = tokio::spawn(async move {
                        let reporter = backend.clone();
                        supervise(
                            backend.run_preparation(spawned_project, prepared, token),
                            move || reporter.diagnostics.report_fatal(),
                        )
                        .await;
                    });
                    if let Some(previous) = state.preparation.take()
                        && let Some(previous_task) = previous.task
                    {
                        previous_task.abort();
                    }
                    state.preparation = Some(Preparation {
                        token,
                        key: key.clone(),
                        phase: PreparationPhase::Running,
                        task: Some(task),
                    });
                }
            }
            #[cfg(test)]
            PREPARATION_GATE.hold(project).await;
            receiver.changed().await.map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "bend-lint preparation stopped with the server",
                )
            })?;
            if self.diagnostics.lint.closed.load(Ordering::SeqCst)
                || self.diagnostics.lint.project_revision(project) != Some(revision)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "bend-lint preparation superseded by newer changes",
                ));
            }
        }
    }

    pub(super) async fn run_preparation(
        &self,
        project: ProjectKey,
        key: PreparationKey,
        token: PreparationToken,
    ) {
        let queued = self.workspace.observability.work.enqueue(WorkKind::Preparation);
        let outcome = match self
            .diagnostics
            .lint
            .prep_semaphore
            .clone()
            .acquire_owned()
            .await
        {
            Ok(permit) => {
                let mut running = CancelOnDrop::new(queued.start());
                let outcome = process::prepare(
                    &key.config,
                    &key.root,
                    self.compiler.reapers.clone(),
                    permit,
                )
                .await;
                if outcome.is_err() {
                    running.fail();
                }
                running.disarm();
                outcome
            }
            Err(error) => Err(std::io::Error::other(error)),
        };
        let mut projects = self.diagnostics.lint.projects.write();
        // Only the attempt that registered this token may publish: a
        // replacement, even one that reuses the same preparation key, keeps its
        // own phase.
        if let Some(state) = projects.get_mut(&project)
            && let Some(entry) = state.preparation.as_mut()
            && entry.token == token
        {
            entry.task = None;
            entry.phase = match outcome {
                Ok(preparation) => PreparationPhase::Ready(preparation.description),
                Err(error) => {
                    PreparationPhase::Failed(format!("bend-lint preparation failed: {error}"))
                }
            };
        }
        drop(projects);
        self.diagnostics
            .lint
            .preparation_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    async fn finish_lint(
        &self,
        capture: LintCapture,
        config: LintConfig,
        project: &ProjectKey,
        revision: u64,
        output: std::process::Output,
    ) -> std::io::Result<String> {
        let uri = &capture.document.uri;
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
        let checked = Arc::clone(&capture.snapshot);
        let permit = self
            .diagnostics
            .lint
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(std::io::Error::other)?;
        let (findings, unchanged) = blocking_result(
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
            epoch: self.diagnostics.lint.next_epoch(),
            project: project.clone(),
            revision,
            revisions: capture.revisions,
            config,
            snapshot: capture.snapshot,
            findings,
        };
        let affected = {
            // Serialize the final revision check/result store against every
            // synchronous project fence; never hold this guard across
            // publication.
            let projects = self.diagnostics.lint.projects.read();
            if !unchanged
                || projects
                    .get(project)
                    .is_none_or(|state| state.revision != revision)
                || !self.diagnostics_snapshot_current(uri, &capture.document.snapshot)
                || !self.lint_result_current_in(&result, &self.workspace_documents(), &projects)
            {
                None
            } else {
                let affected = self
                    .diagnostics
                    .lint
                    .results
                    .write()
                    .insert(uri.clone(), Arc::new(result));
                self.diagnostics.lint.control.record(
                    uri,
                    LintPhase::Ready,
                    completion.clone(),
                    revision,
                );
                Some(affected)
            }
        };
        let Some(affected) = affected else {
            self.finish_lint_control(
                uri,
                LintPhase::Skipped,
                "Project inputs changed during analysis; the next check will re-read them",
                project,
                revision,
            )
            .await;
            return Ok("Analysis skipped; inputs changed during analysis".into());
        };
        self.publish_coordinated_locked(affected).await;
        Ok(completion)
    }
}
