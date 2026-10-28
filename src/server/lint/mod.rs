mod actions;
mod config;
pub(super) mod control;
mod process;
mod project;
mod results;
mod snapshot;
mod wire;
pub(super) use actions::DANGEROUS_KIND;
use config::LintConfig;
use control::{LintControl, LintPhase};
use project::{Preparation, PreparationKey, PreparationPhase, ProjectKey, ProjectState};
use results::LintResults;

use super::{lsp::Backend, state::State};
use crate::workspace::{Document, FileId, WorkspaceDb, normalize_path};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
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
use tower_lsp::lsp_types::{MessageType, NumberOrString};
use url::Url;

// Long enough to absorb a burst of keystrokes, short enough that a stopped
// editor sees refreshed diagnostics without an explicit command.
const LINT_DEBOUNCE: Duration = Duration::from_millis(250);

const WORKSPACE_REASON: &str = "Workspace folders changed; the next check will re-read them";
const AFFECTED_REASON: &str = "Inputs changed; the affected results will be rechecked";
const SUPERSEDED_REASON: &str = "Superseded by newer editor changes";
const CANCELLED_REASON: &str = "Cancelled; the current inputs will be rechecked";
const FROZEN_REASON: &str = "Project inputs changed without an accepted event; the current revision's view is frozen and the next check will re-read them";

pub(super) struct LintResult {
    epoch: u64,
    project: ProjectKey,
    // Project revision that produced this entry. Cleanup may only retire
    // results from revisions an older plan actually fenced.
    revision: u64,
    revisions: Vec<(Url, u64)>,
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
    generations: HashMap<Url, u64>,
}

struct LintCapture {
    document: Document,
    revisions: Vec<(Url, u64)>,
    snapshot: Arc<snapshot::ProjectSnapshot>,
}

impl LintInputs {
    fn fences(&self, view: &snapshot::ProjectSnapshot) -> Vec<(Url, u64)> {
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
                self.generations
                    .get(&document.uri)
                    .map(|generation| (document.uri.clone(), *generation))
            })
            .collect()
    }
}

pub(super) struct LintService {
    config: State<LintConfig>,
    results: State<LintResults>,
    control: LintControl,
    projects: State<HashMap<ProjectKey, ProjectState>>,
    preparation_changed: watch::Sender<u64>,
    prep_semaphore: Arc<Semaphore>,
    epoch: AtomicU64,
    progress: AtomicU64,
    next_task: AtomicU64,
    closed: AtomicBool,
    semaphore: Arc<Semaphore>,
}

struct ProjectFence {
    key: ProjectKey,
    retired_revision: u64,
    previous: Option<JoinHandle<()>>,
    progress: Vec<NumberOrString>,
    preserved: BTreeSet<Url>,
}

struct LintPlan {
    change: LintChange,
    fences: Vec<ProjectFence>,
    affected: HashSet<Url>,
    // Entries discovered through reverse imports. They are scheduled in this
    // plan's revision even when their project was already fenced.
    entries: BTreeSet<Url>,
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

    fn next_task(&self) -> u64 {
        self.next_task.fetch_add(1, Ordering::SeqCst)
    }

    fn take_results(&self) -> HashSet<Url> {
        self.results.write().clear()
    }

    fn project_revision(&self, key: &ProjectKey) -> Option<u64> {
        self.projects.read().get(key).map(|state| state.revision)
    }

    // Fence only the routed projects: bump their revision, retire their
    // scheduler while handing every queued entry back, drop the cached view,
    // and (for settings) abort only preparations whose analyzer key changed.
    // Results are removed after the mutation commits so a rejected event
    // cannot erase a newer coherent result.
    fn fence_projects(
        &self,
        keys: impl IntoIterator<Item = ProjectKey>,
        cancel_preparation: Option<&LintConfig>,
    ) -> Vec<ProjectFence> {
        let mut projects = self.projects.write();
        let results = self.results.read();
        let mut fences = Vec::new();
        for key in keys {
            let state = projects.entry(key.clone()).or_default();
            let retired_revision = state.revision;
            state.revision = state.revision.wrapping_add(1);
            state.view = None;
            state.completed.clear();
            let previous = state.task.take().map(|(_, task)| task);
            if let Some(task) = &previous {
                task.abort();
            }
            let progress = self
                .control
                .take_project_revision_progress(&key, retired_revision);
            let mut preserved = std::mem::take(&mut state.pending);
            if let Some(active) = state.active.take() {
                preserved.insert(active);
            }
            preserved.extend(results.entries_of(&key));
            if let Some(config) = cancel_preparation
                && let Some(preparation) = state.preparation.take()
            {
                if preparation.key.config == *config {
                    state.preparation = Some(preparation);
                } else if let Some(task) = preparation.task {
                    task.abort();
                }
            }
            fences.push(ProjectFence {
                key,
                retired_revision,
                previous,
                progress,
                preserved,
            });
        }
        fences
    }

    fn reset_failed_preparation(&self, key: &ProjectKey) {
        let mut projects = self.projects.write();
        let Some(state) = projects.get_mut(key) else {
            return;
        };
        if !state
            .preparation
            .as_ref()
            .is_some_and(|entry| matches!(&entry.phase, PreparationPhase::Failed(_)))
        {
            return;
        }
        if let Some(preparation) = state.preparation.take()
            && let Some(task) = preparation.task
        {
            task.abort();
        }
        drop(projects);
        self.preparation_changed
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    // Only Settings can change the provider while it is disabled; every other
    // event is a no-op for lint, keeping the default-disabled hot path free.
    fn tracks(&self, change: &LintChange) -> bool {
        !self.closed.load(Ordering::SeqCst)
            && (matches!(change, LintChange::Settings(_)) || self.config.read().enabled)
    }

    fn affected_results<'a>(&self, keys: impl IntoIterator<Item = &'a ProjectKey>) -> HashSet<Url> {
        let results = self.results.read();
        let mut affected = HashSet::new();
        for key in keys {
            affected.extend(results.affected_of(key));
        }
        affected
    }
}

impl Backend {
    pub(super) async fn shutdown_lint(&self) {
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

    fn lint_revisions_current(&self, revisions: &[(Url, u64)]) -> bool {
        let state = self.workspace.state.read();
        revisions
            .iter()
            .all(|(uri, generation)| state.revision_current(uri, *generation))
    }

    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "open_inputs", result_count = tracing::field::Empty))]
    fn lint_inputs(&self) -> LintInputs {
        let state = self.workspace.state.read();
        let documents = state.database.open_documents();
        tracing::Span::current().record("result_count", documents.len());
        let generations = documents
            .iter()
            .filter_map(|document| {
                state
                    .revision_generation(&document.uri)
                    .map(|generation| (document.uri.clone(), generation))
            })
            .collect();
        LintInputs {
            documents,
            generations,
        }
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
        let key = Self::project_key(uri);
        let projects = self.diagnostics.lint.projects.read();
        match projects
            .get(&key)
            .and_then(|state| state.preparation.as_ref())
        {
            Some(entry) if entry.key.config == *config => match &entry.phase {
                PreparationPhase::Ready(description) => description.clone(),
                PreparationPhase::Running => "Preparing the analyzer and its pinned compiler; the identity is reported when preparation completes.".into(),
                PreparationPhase::Failed(message) => message.clone(),
            },
            _ => "Not prepared yet; the first check validates the analyzer contract and reports its identity.".into(),
        }
    }

    fn enqueue_project(&self, key: ProjectKey, entries: BTreeSet<Url>, debounce: bool) {
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
        let handle = tokio::spawn(async move {
            let reporter = backend.clone();
            let operation = async move {
                if debounce {
                    tokio::time::sleep(LINT_DEBOUNCE).await;
                }
                backend.run_project(spawned_key, revision, task).await;
            };
            super::state::supervise(operation, move || reporter.diagnostics.report_fatal()).await;
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
            if let Some(document) = self.document(&uri).filter(Self::supported) {
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
        let current = super::state::blocking_result(
            tokio::task::spawn_blocking(move || checked.unchanged()).await,
        );
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
                        match reusable.filter(|snapshot| snapshot.refreshable(&capture_file)) {
                            Some(snapshot) => Arc::new(snapshot.with_overlays(&documents)?),
                            None => Arc::new(snapshot::ProjectSnapshot::capture(
                                &capture_file,
                                &documents,
                            )?),
                        }
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
            revisions: inputs.fences(&snapshot),
            document,
            snapshot,
        })
    }

    async fn ensure_prepared(
        &self,
        project: &ProjectKey,
        revision: u64,
        key: PreparationKey,
    ) -> std::io::Result<String> {
        loop {
            let mut receiver = self.diagnostics.lint.preparation_changed.subscribe();
            let start = {
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
                match state
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
                }
            };
            if start {
                let backend = self.clone();
                let prepared = key.clone();
                let spawned_project = project.clone();
                let task = tokio::spawn(async move {
                    let reporter = backend.clone();
                    super::state::supervise(
                        backend.run_preparation(spawned_project, prepared),
                        move || reporter.diagnostics.report_fatal(),
                    )
                    .await;
                });
                let mut projects = self.diagnostics.lint.projects.write();
                let Some(state) = projects.get_mut(project) else {
                    task.abort();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "bend-lint project closed while preparing",
                    ));
                };
                if state.revision != revision {
                    task.abort();
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "bend-lint preparation superseded by newer changes",
                    ));
                }
                if let Some(previous) = state.preparation.take()
                    && let Some(previous_task) = previous.task
                {
                    previous_task.abort();
                }
                state.preparation = Some(Preparation {
                    key: key.clone(),
                    phase: PreparationPhase::Running,
                    task: Some(task),
                });
            }
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

    async fn run_preparation(&self, project: ProjectKey, key: PreparationKey) {
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
        let mut projects = self.diagnostics.lint.projects.write();
        if let Some(state) = projects.get_mut(&project)
            && let Some(entry) = state.preparation.as_mut()
            && entry.key == key
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

// The complete editor/workspace surface that can change lint inputs. Handlers
// describe what happened and hand over their mutation future; routing,
// fencing, cleanup, and scheduling stay inside this module.
pub(in crate::server) enum LintChange {
    Open { uri: Url, revision: i32 },
    Change { uri: Url, revision: i32 },
    Save { uri: Url },
    Close { uri: Url },
    Settings(LintSettings),
    Watched(Vec<PathBuf>),
    Workspace,
    Rename(Vec<(PathBuf, PathBuf)>),
    Run { uri: Url },
}

pub(in crate::server) struct LintSettings {
    config: LintConfig,
    warning: Option<String>,
}

impl LintChange {
    // Parse `bend.lint` exactly once from the borrowed settings object: no
    // whole-settings JSON clone and no second parse in the coordinator.
    pub(in crate::server) fn settings(settings: &serde_json::Value) -> Self {
        Self::Settings(LintSettings::parse(settings))
    }
}

impl LintSettings {
    fn parse(settings: &serde_json::Value) -> Self {
        match LintConfig::from_settings(settings) {
            Ok(config) => Self {
                config,
                warning: None,
            },
            Err(message) => Self {
                config: LintConfig::default(),
                warning: Some(format!("bend-lint configuration error: {message}")),
            },
        }
    }
}

// The coordinator entry for every editor/workspace event that can change lint
// inputs. Fencing happens synchronously before the mutation so no captured view
// can publish across it; the operation is awaited before any lint cleanup, and
// only the routed projects are retired or scheduled afterwards.
impl Backend {
    pub(super) async fn apply_lint_change<F>(&self, change: LintChange, operation: F) -> bool
    where
        F: Future<Output = bool>,
    {
        if !self.diagnostics.lint.tracks(&change) {
            return operation.await;
        }
        let plan = self.begin_lint_change(change);
        let applied = operation.await;
        self.finish_lint_change(plan, applied).await;
        applied
    }

    fn begin_lint_change(&self, change: LintChange) -> LintPlan {
        let (fences, affected, entries) = match &change {
            LintChange::Settings(settings) => {
                let config = settings.config.clone();
                self.diagnostics
                    .lint
                    .control
                    .configure(&config, settings.warning.clone());
                *self.diagnostics.lint.config.write() = config.clone();
                let keys = self
                    .diagnostics
                    .lint
                    .projects
                    .read()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                let fences = self.diagnostics.lint.fence_projects(keys, Some(&config));
                let affected = self.diagnostics.lint.results.read().affected_keys();
                (fences, affected, BTreeSet::new())
            }
            LintChange::Workspace => {
                self.diagnostics.lint.control.invalidate(WORKSPACE_REASON);
                let keys = self
                    .diagnostics
                    .lint
                    .projects
                    .read()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>();
                let fences = self.diagnostics.lint.fence_projects(keys, None);
                let affected = self.diagnostics.lint.results.read().affected_keys();
                (fences, affected, BTreeSet::new())
            }
            LintChange::Open { uri, .. } | LintChange::Change { uri, .. } => {
                let dependents = self.dependent_documents_of(uri);
                let keys = self.affected_project_keys_with(uri, &dependents);
                let fences = self.diagnostics.lint.fence_projects(keys, None);
                self.invalidate_for(&fences, Some(uri), AFFECTED_REASON);
                (fences, HashSet::new(), dependents.into_iter().collect())
            }
            LintChange::Save { uri } => {
                let dependents = self.dependent_documents_of(uri);
                let keys = self.affected_project_keys_with(uri, &dependents);
                let fences = self.diagnostics.lint.fence_projects(keys, None);
                self.invalidate_for(&fences, Some(uri), SUPERSEDED_REASON);
                (fences, HashSet::new(), dependents.into_iter().collect())
            }
            LintChange::Run { uri } => {
                let keys = self.affected_project_keys(std::slice::from_ref(uri));
                for key in &keys {
                    self.diagnostics.lint.reset_failed_preparation(key);
                }
                let fences = self.diagnostics.lint.fence_projects(keys, None);
                self.invalidate_for(&fences, Some(uri), SUPERSEDED_REASON);
                (fences, HashSet::new(), BTreeSet::new())
            }
            LintChange::Close { uri } => {
                let dependents = self.dependent_documents_of(uri);
                let keys = self.affected_project_keys_with(uri, &dependents);
                self.diagnostics.lint.control.forget(uri);
                let fences = self.diagnostics.lint.fence_projects(keys.clone(), None);
                self.invalidate_for(&fences, Some(uri), AFFECTED_REASON);
                let affected = self.diagnostics.lint.affected_results(&keys);
                (fences, affected, dependents.into_iter().collect())
            }
            LintChange::Watched(paths) => {
                let dependents = self.dependent_documents_for_paths(paths);
                let mut keys = self.affected_projects(paths);
                keys.extend(self.affected_project_keys(&dependents));
                let fences = self.diagnostics.lint.fence_projects(keys.clone(), None);
                self.invalidate_for(&fences, None, AFFECTED_REASON);
                let affected = self.diagnostics.lint.affected_results(&keys);
                (fences, affected, dependents.into_iter().collect())
            }
            LintChange::Rename(pairs) => {
                let paths: Vec<PathBuf> = pairs
                    .iter()
                    .flat_map(|(old, new)| [old.clone(), new.clone()])
                    .collect();
                let dependents = self.dependent_documents_for_paths(&paths);
                let mut keys = self.affected_projects(&paths);
                keys.extend(self.affected_project_keys(&dependents));
                let fences = self.diagnostics.lint.fence_projects(keys.clone(), None);
                self.invalidate_for(&fences, None, AFFECTED_REASON);
                let affected = self.diagnostics.lint.affected_results(&keys);
                (fences, affected, dependents.into_iter().collect())
            }
        };
        LintPlan {
            change,
            fences,
            affected,
            entries,
        }
    }

    fn invalidate_for(&self, fences: &[ProjectFence], extra: Option<&Url>, reason: &str) {
        let mut retired: Vec<(&Url, u64)> = Vec::new();
        for fence in fences {
            retired.extend(
                fence
                    .preserved
                    .iter()
                    .map(|uri| (uri, fence.retired_revision)),
            );
        }
        if let Some(uri) = extra {
            let key = Self::project_key(uri);
            let revision = fences
                .iter()
                .find(|fence| fence.key == key)
                .map_or(u64::MAX, |fence| fence.retired_revision);
            retired.push((uri, revision));
        }
        self.control().invalidate_selected(retired, reason);
    }

    fn control(&self) -> &LintControl {
        &self.diagnostics.lint.control
    }

    async fn finish_lint_change(&self, plan: LintPlan, applied: bool) {
        let mut fences = plan.fences;
        self.retire_fences(&mut fences).await;
        if !applied {
            // A rejected mutation owns nothing: never abort or replace a newer
            // scheduler and never remove results; hand preserved entries back
            // so the newer or next owner drains them.
            self.restore_fences(fences);
            return;
        }
        let mut affected = plan.affected;
        let mut routed = plan.entries;
        // A newly opened document, a created input, or a renamed path is
        // interned by the mutation that just completed, so its reverse imports
        // are only visible here. Fence and re-run those importer projects;
        // otherwise their completed entries would suppress the only check that
        // can read the new input identity.
        let dependents = match &plan.change {
            LintChange::Open { uri, .. } => self.dependent_documents_of(uri),
            LintChange::Watched(paths) => self.dependent_documents_for_paths(paths),
            LintChange::Rename(pairs) => {
                let paths: Vec<PathBuf> = pairs
                    .iter()
                    .flat_map(|(old, new)| [old.clone(), new.clone()])
                    .collect();
                self.dependent_documents_for_paths(&paths)
            }
            LintChange::Settings(_)
            | LintChange::Workspace
            | LintChange::Change { .. }
            | LintChange::Save { .. }
            | LintChange::Run { .. }
            | LintChange::Close { .. } => Vec::new(),
        };
        if !dependents.is_empty() {
            routed.extend(dependents.iter().cloned());
            let planned: BTreeSet<ProjectKey> =
                fences.iter().map(|fence| fence.key.clone()).collect();
            let keys: BTreeSet<ProjectKey> = self
                .affected_project_keys(&dependents)
                .into_iter()
                .filter(|key| !planned.contains(key))
                .collect();
            if !keys.is_empty() {
                let mut extra = self.diagnostics.lint.fence_projects(keys, None);
                self.retire_fences(&mut extra).await;
                self.invalidate_for(&extra, None, AFFECTED_REASON);
                affected.extend(self.remove_retired_lint_results(&extra));
                fences.extend(extra);
            }
        }
        match &plan.change {
            LintChange::Settings(_) => {
                let config = self.diagnostics.lint.config.read().clone();
                affected = self
                    .diagnostics
                    .lint
                    .results
                    .write()
                    .remove_stale(|result| result.config != config);
            }
            LintChange::Workspace => {
                affected = self.remove_retired_lint_results(&fences);
            }
            LintChange::Close { .. } | LintChange::Watched(_) | LintChange::Rename(_) => {
                affected.extend(self.remove_retired_lint_results(&fences));
            }
            LintChange::Open { .. }
            | LintChange::Change { .. }
            | LintChange::Save { .. }
            | LintChange::Run { .. } => {}
        }
        if !affected.is_empty() {
            self.publish_coordinated_diagnostics(affected).await;
        }
        if let LintChange::Settings(settings) = &plan.change
            && let Some(warning) = &settings.warning
        {
            self.client.log_message(MessageType::WARNING, warning).await;
            self.client
                .show_message(MessageType::WARNING, warning.clone())
                .await;
        }
        self.schedule_change(&plan.change, &fences, &routed);
        if matches!(
            plan.change,
            LintChange::Close { .. }
                | LintChange::Watched(_)
                | LintChange::Rename(_)
                | LintChange::Workspace
        ) {
            self.prune_unused_projects();
        }
    }

    fn remove_retired_lint_results(&self, fences: &[ProjectFence]) -> HashSet<Url> {
        let mut results = self.diagnostics.lint.results.write();
        let mut removed = HashSet::new();
        for fence in fences {
            removed.extend(results.remove_project_revision(&fence.key, fence.retired_revision));
        }
        removed
    }

    async fn retire_fences(&self, fences: &mut [ProjectFence]) {
        for fence in fences {
            join_lint_task(fence.previous.take()).await;
            let mut progress = std::mem::take(&mut fence.progress);
            progress.extend(
                self.control()
                    .take_project_revision_progress(&fence.key, fence.retired_revision),
            );
            self.end_lint_progress_tokens(progress, CANCELLED_REASON)
                .await;
        }
    }

    fn restore_fences(&self, fences: Vec<ProjectFence>) {
        if !self.diagnostics.lint.config.read().enabled
            || self.diagnostics.lint.closed.load(Ordering::SeqCst)
        {
            return;
        }
        let entries: BTreeSet<Url> = fences
            .into_iter()
            .flat_map(|fence| fence.preserved)
            .collect();
        self.schedule_entries(entries, true);
    }

    fn schedule_change(
        &self,
        change: &LintChange,
        fences: &[ProjectFence],
        routed: &BTreeSet<Url>,
    ) {
        match change {
            LintChange::Settings(_) | LintChange::Workspace => {
                let entries: BTreeSet<Url> = self
                    .workspace_documents()
                    .into_iter()
                    .filter(Self::supported)
                    .map(|document| document.uri)
                    .collect();
                self.schedule_entries(entries, true);
            }
            LintChange::Open { uri, revision } | LintChange::Change { uri, revision } => {
                let current = self
                    .document(uri)
                    .is_some_and(|document| document.revision.0 == *revision);
                let mut entries = Self::fence_entries(fences);
                entries.extend(routed.iter().cloned());
                if current {
                    entries.insert(uri.clone());
                }
                self.schedule_entries(entries, true);
            }
            LintChange::Save { uri } => {
                let mut entries = Self::fence_entries(fences);
                entries.extend(routed.iter().cloned());
                entries.insert(uri.clone());
                self.schedule_entries(entries, false);
            }
            LintChange::Close { uri } => {
                let mut entries = Self::fence_entries(fences);
                entries.extend(routed.iter().cloned());
                // The closed spelling may be the only queued/retained entry
                // (for example one half of a conflicted view). Surviving open
                // entries of the affected project must still be rechecked.
                entries.extend(self.fence_open_entries(fences));
                entries.remove(uri);
                self.schedule_entries(entries, true);
            }
            LintChange::Watched(_) | LintChange::Rename(_) => {
                let mut entries = Self::fence_entries(fences);
                entries.extend(routed.iter().cloned());
                self.schedule_entries(entries, true);
            }
            LintChange::Run { uri } => {
                let mut entries = Self::fence_entries(fences);
                entries.insert(uri.clone());
                self.schedule_entries(entries, false);
            }
        }
    }

    fn fence_entries(fences: &[ProjectFence]) -> BTreeSet<Url> {
        fences
            .iter()
            .flat_map(|fence| fence.preserved.iter().cloned())
            .collect()
    }

    fn schedule_entries(&self, entries: impl IntoIterator<Item = Url>, debounce: bool) {
        if !self.diagnostics.lint.config.read().enabled
            || self.diagnostics.lint.closed.load(Ordering::SeqCst)
        {
            return;
        }
        let mut groups: BTreeMap<ProjectKey, BTreeSet<Url>> = BTreeMap::new();
        for uri in entries {
            if !self.document(&uri).as_ref().is_some_and(Self::supported) {
                continue;
            }
            groups
                .entry(Self::project_key(&uri))
                .or_default()
                .insert(uri);
        }
        for (key, entries) in groups {
            self.enqueue_project(key, entries, debounce);
        }
    }

    fn project_key(uri: &Url) -> ProjectKey {
        uri.to_file_path()
            .ok()
            .and_then(|path| snapshot::project_root(&path))
            .map_or_else(
                || ProjectKey::Document(uri.clone()),
                |root| Self::root_key(&root),
            )
    }

    fn root_key(root: &Path) -> ProjectKey {
        ProjectKey::Root(snapshot::input_identity(root).unwrap_or_else(|| normalize_path(root)))
    }

    fn affected_project_keys(&self, uris: &[Url]) -> BTreeSet<ProjectKey> {
        let mut keys = BTreeSet::new();
        let mut paths = Vec::new();
        for uri in uris {
            keys.insert(Self::project_key(uri));
            if let Ok(path) = uri.to_file_path() {
                paths.push(path.clone());
                if let Some(identity) = snapshot::input_identity(&path) {
                    paths.push(identity);
                }
            }
        }
        keys.extend(self.covering_projects(&paths));
        keys
    }

    fn affected_projects(&self, paths: &[PathBuf]) -> BTreeSet<ProjectKey> {
        let mut keys = self.covering_projects(paths);
        for path in paths {
            if let Some(root) = snapshot::project_root(path) {
                keys.insert(Self::root_key(&root));
            }
            if let Some(identity) = snapshot::input_identity(path)
                && let Some(root) = snapshot::project_root(&identity)
            {
                keys.insert(Self::root_key(&root));
            }
        }
        keys
    }

    fn fence_open_entries(&self, fences: &[ProjectFence]) -> BTreeSet<Url> {
        let keys: BTreeSet<ProjectKey> = fences.iter().map(|fence| fence.key.clone()).collect();
        self.workspace_documents()
            .into_iter()
            .filter(Self::supported)
            .filter(|document| keys.contains(&Self::project_key(&document.uri)))
            .map(|document| document.uri)
            .collect()
    }

    // A document event also reaches every project that reverse-imports it, so an
    // external or aliased dependency fences and re-runs its importer even when
    // the changed spelling is not part of any retained snapshot.
    fn affected_project_keys_with(&self, uri: &Url, dependents: &[Url]) -> BTreeSet<ProjectKey> {
        let mut uris = Vec::with_capacity(dependents.len() + 1);
        uris.push(uri.clone());
        uris.extend(dependents.iter().cloned());
        self.affected_project_keys(&uris)
    }

    fn dependent_documents_of(&self, uri: &Url) -> Vec<Url> {
        let database = self.workspace.read();
        let id = database.file_id_by_uri(uri).or_else(|| {
            uri.to_file_path()
                .ok()
                .and_then(|path| Self::file_id_by_path(&database, &path))
        });
        let Some(id) = id else {
            return Vec::new();
        };
        database
            .dependents(id)
            .into_iter()
            .map(|document| document.uri)
            .collect()
    }

    // Discovery can intern a dependency under a canonical parent directory
    // while an editor or watcher spells the same file through a symlinked
    // parent. Retrying the identity-qualified parent spelling bridges only that
    // difference; the leaf name stays the captured alias key.
    fn file_id_by_path(database: &WorkspaceDb, path: &Path) -> Option<FileId> {
        database.file_id_by_path(path).or_else(|| {
            let name = path.file_name()?;
            let parent = snapshot::input_identity(path.parent()?)?;
            database.file_id_by_path(&parent.join(name))
        })
    }

    // Filesystem events name paths, including spellings that are not project
    // members (an external import alias). Their importers still identify the
    // projects that must re-read the changed identity.
    fn dependent_documents_for_paths(&self, paths: &[PathBuf]) -> Vec<Url> {
        let mut dependents = Vec::new();
        let database = self.workspace.read();
        for path in paths {
            if let Some(id) = Self::file_id_by_path(&database, path) {
                dependents.extend(database.dependents(id).into_iter().map(|doc| doc.uri));
            }
        }
        dependents
    }

    fn covering_projects(&self, paths: &[PathBuf]) -> BTreeSet<ProjectKey> {
        let mut keys: BTreeSet<ProjectKey> = self
            .diagnostics
            .lint
            .results
            .read()
            .projects_covering(paths)
            .into_iter()
            .collect();
        let projects = self.diagnostics.lint.projects.read();
        for key in projects.keys() {
            if key.involves(paths) {
                keys.insert(key.clone());
            }
        }
        keys
    }

    fn prune_unused_projects(&self) {
        let keys: Vec<ProjectKey> = self
            .diagnostics
            .lint
            .projects
            .read()
            .keys()
            .cloned()
            .collect();
        let documents = self.workspace_documents();
        for key in keys {
            let mut projects = self.diagnostics.lint.projects.write();
            let Some(state) = projects.get(&key) else {
                continue;
            };
            let busy = !state.pending.is_empty()
                || state.active.is_some()
                || documents
                    .iter()
                    .any(|document| Self::project_key(&document.uri) == key);
            let retained = !self
                .diagnostics
                .lint
                .results
                .read()
                .entries_of(&key)
                .is_empty();
            if busy || retained {
                continue;
            }
            if let Some(mut state) = projects.remove(&key)
                && let Some(preparation) = state.preparation.take()
                && let Some(task) = preparation.task
            {
                task.abort();
            }
        }
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
        Backend, DANGEROUS_KIND, LintChange, LintConfig, LintResult, control::LintPhase, snapshot,
        wire,
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
    use tokio::sync::{Notify, Semaphore};
    use tower_lsp::{
        LanguageServer, LspService,
        lsp_types::{
            CodeAction, CodeActionOrCommand, CodeActionParams, Diagnostic, DiagnosticSeverity,
            DocumentChanges, ExecuteCommandParams, OneOf, Position, Range,
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

    fn fence(backend: &Backend, uri: &Url) -> Result<(Url, u64), Box<dyn Error>> {
        let generation = backend
            .workspace
            .state
            .read()
            .revision_generation(uri)
            .ok_or_else(|| io::Error::other("open input revision"))?;
        Ok((uri.clone(), generation))
    }

    fn retain(
        backend: &Backend,
        uri: &Url,
        snapshot: &Arc<snapshot::ProjectSnapshot>,
        revisions: Vec<(Url, u64)>,
    ) {
        let epoch = backend.diagnostics.lint.epoch.load(Ordering::SeqCst);
        retain_entry(
            backend,
            uri,
            entry(backend, epoch, snapshot, revisions, no_findings()),
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
        revisions: Vec<(Url, u64)>,
        findings: wire::Findings,
    ) -> Arc<LintResult> {
        entry_at(backend, epoch, 0, snapshot, revisions, findings)
    }

    fn entry_at(
        backend: &Backend,
        epoch: u64,
        revision: u64,
        snapshot: &Arc<snapshot::ProjectSnapshot>,
        revisions: Vec<(Url, u64)>,
        findings: wire::Findings,
    ) -> Arc<LintResult> {
        Arc::new(LintResult {
            epoch,
            project: Backend::root_key(&snapshot.root),
            revision,
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
        retain(&backend, &project.main_uri, &snapshot, revisions);
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
        retain(&backend, &project.main_uri, &snapshot, revisions);
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
        retain(&backend, &project.main_uri, &snapshot, revisions);
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

    async fn status_state(backend: &Backend, uri: &Url) -> Result<String, Box<dyn Error>> {
        let result = backend
            .execute_command(ExecuteCommandParams {
                command: "bend2.lint.status".into(),
                arguments: vec![serde_json::json!({"uri": uri})],
                work_done_progress_params: tower_lsp::lsp_types::WorkDoneProgressParams::default(),
            })
            .await?;
        Ok(result
            .and_then(|value| value["state"].as_str().map(str::to_owned))
            .unwrap_or_default())
    }

    fn record_ready(backend: &Backend, uri: &Url, reason: &str, revision: u64) {
        backend
            .diagnostics
            .lint
            .control
            .record(uri, LintPhase::Ready, reason, revision);
    }

    fn versioned_action(
        actions: &[CodeActionOrCommand],
        uri: &Url,
        version: i32,
        message: &str,
    ) -> bool {
        dangerous_actions(actions).iter().any(|action| {
            action.diagnostics.as_ref().is_some_and(|diagnostics| {
                diagnostics
                    .iter()
                    .any(|diagnostic| diagnostic.message == message)
            }) && action
                .edit
                .as_ref()
                .and_then(|edit| edit.document_changes.as_ref())
                .is_some_and(|changes| match changes {
                    DocumentChanges::Edits(edits) => edits.iter().any(|edit| {
                        edit.text_document.uri == *uri
                            && edit.text_document.version == Some(version)
                    }),
                    DocumentChanges::Operations(_) => false,
                })
        })
    }

    fn pause_committed_close(
        backend: Backend,
        uri: Url,
        reached: Arc<Notify>,
        release: Arc<Notify>,
    ) -> tokio::task::JoinHandle<bool> {
        let mutation = backend.clone();
        let close_uri = uri.clone();
        tokio::spawn(async move {
            backend
                .apply_lint_change(LintChange::Close { uri: close_uri }, async move {
                    let Some(closed) = mutation.workspace.begin_close(&uri) else {
                        return false;
                    };
                    if !mutation.close_workspace_document(uri, closed).await {
                        return false;
                    }
                    reached.notify_one();
                    release.notified().await;
                    true
                })
                .await
        })
    }

    // A committed mutation can await before its plan finishes. A newer
    // revision that publishes during that window owns its versioned action and
    // status; the older plan's cleanup may only retire results from its own
    // revision.
    #[tokio::test]
    async fn late_close_cleanup_keeps_a_newer_versioned_action_and_ready_status()
    -> Result<(), Box<dyn Error>> {
        let project = project()?;
        let backend = backend()?;
        open(&backend, &project.main_uri, 1, MAIN, project.main.clone()).await?;
        open(&backend, &project.dep_uri, 1, DEP, project.dep.clone()).await?;
        // The old plan must have a real same-project result to own and replace.
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        retain_entry(
            &backend,
            &project.main_uri,
            entry(
                &backend,
                1,
                &snapshot,
                revisions,
                actionable("older rule", &project.main_uri),
            ),
        );
        let params = dangerous_params(&project.main_uri)?;
        assert!(
            versioned_action(
                &backend.lint_code_actions(&params).await,
                &project.main_uri,
                1,
                "older rule"
            ),
            "the older revision's versioned action exists before the close starts"
        );
        let reached = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let close = pause_committed_close(
            backend.clone(),
            project.main_uri.clone(),
            Arc::clone(&reached),
            Arc::clone(&release),
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), reached.notified()).await?;
        // The close is committed and the older plan is paused before its
        // cleanup. Reopen through the real workspace callback, then complete a
        // newer revision over the current open view.
        open(&backend, &project.main_uri, 3, MAIN, project.main.clone()).await?;
        let snapshot = fixture(&backend, &project)?;
        let revisions = vec![
            fence(&backend, &project.main_uri)?,
            fence(&backend, &project.dep_uri)?,
        ];
        let key = Backend::root_key(&snapshot.root);
        let newer_revision = backend
            .diagnostics
            .lint
            .project_revision(&key)
            .ok_or_else(|| io::Error::other("project revision"))?;
        retain_entry(
            &backend,
            &project.main_uri,
            entry_at(
                &backend,
                2,
                newer_revision,
                &snapshot,
                revisions,
                actionable("newer rule", &project.main_uri),
            ),
        );
        record_ready(
            &backend,
            &project.main_uri,
            "newer analysis",
            newer_revision,
        );
        assert!(
            versioned_action(
                &backend.lint_code_actions(&params).await,
                &project.main_uri,
                3,
                "newer rule"
            ),
            "the newer revision's versioned action is observable before the older plan finishes"
        );
        assert_eq!(status_state(&backend, &project.main_uri).await?, "ready");
        release.notify_one();
        let applied = close.await?;
        assert!(applied);
        assert!(
            versioned_action(
                &backend.lint_code_actions(&params).await,
                &project.main_uri,
                3,
                "newer rule"
            ),
            "late cleanup from an older plan must not delete the newer versioned action"
        );
        assert_eq!(status_state(&backend, &project.main_uri).await?, "ready");
        Ok(())
    }
}
