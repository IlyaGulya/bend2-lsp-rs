use super::{
    compiler::compiler_diagnostics, features::lexical_diagnostics, lsp::Backend, state::State,
};
use crate::{
    analysis::Revision,
    workspace::{Document, FileId, SourceGraph, normalize_path},
};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex, Semaphore, watch},
    task::{JoinError, JoinHandle},
    time::sleep,
};
use tower_lsp::lsp_types::{Diagnostic, MessageType, NumberOrString};
use tracing::Instrument;
use url::Url;

#[derive(Clone)]
pub(super) struct ImportedDiagnostics {
    version: Option<i32>,
    pub(super) by_uri: HashMap<Url, Vec<Diagnostic>>,
    retired: HashSet<Url>,
    context: Option<Arc<DiagnosticsContext>>,
}

#[derive(Default)]
struct TaskRegistry {
    closed: bool,
    next_generation: u64,
    tasks: HashMap<Url, (u64, JoinHandle<()>)>,
}

#[derive(Clone)]
pub(super) struct DocumentDiagnostics {
    snapshot: Arc<crate::analysis::DocumentSnapshot>,
    diagnostics: Vec<Diagnostic>,
    context: Arc<DiagnosticsContext>,
}

struct DiagnosticsContext {
    epoch: u64,
    root: Option<RootView>,
    sources: Vec<SourceIdentity>,
}

/// Requested editor spelling that owns the graph root. Its own buffer and LSP
/// version stream publish that root's result; other spellings of the same file
/// only supply the representative text and revision stream for dependencies.
struct RootView {
    id: FileId,
    uri: Url,
}

struct SourceIdentity {
    id: FileId,
    snapshot: Option<Arc<crate::analysis::DocumentSnapshot>>,
    disk_generation: Option<u64>,
}

pub(super) struct DiagnosticsService {
    registry: Mutex<TaskRegistry>,
    fatal: watch::Sender<bool>,
    pub(super) imported: State<HashMap<Url, ImportedDiagnostics>>,
    pub(super) imported_publish: Mutex<()>,
    pub(super) analysis: Arc<Semaphore>,
    pub(super) documents: State<HashMap<Url, DocumentDiagnostics>>,
    pub(super) lint: super::lint::LintService,
    pub(super) context_epoch: AtomicU64,
}

impl Default for DiagnosticsService {
    fn default() -> Self {
        Self {
            registry: Mutex::new(TaskRegistry::default()),
            fatal: watch::channel(false).0,
            imported: State::new(HashMap::new()),
            imported_publish: Mutex::new(()),
            analysis: Arc::new(Semaphore::new(4)),
            documents: State::new(HashMap::new()),
            lint: super::lint::LintService::default(),
            context_epoch: AtomicU64::new(0),
        }
    }
}

impl DiagnosticsService {
    pub(super) fn invalidate_context(&self) {
        self.context_epoch
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |epoch| {
                epoch.checked_add(1)
            })
            .unwrap_or_else(|_| panic!("diagnostics context epoch exhausted"));
    }

    pub(super) fn fatal_receiver(&self) -> watch::Receiver<bool> {
        self.fatal.subscribe()
    }

    pub(super) fn report_fatal(&self) {
        self.fatal.send_replace(true);
    }

    fn observe_task_result(&self, result: Result<(), JoinError>) {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            tracing::error!(%error, "diagnostics task failed; server restart required");
            self.report_fatal();
        }
    }

    /// Admission, generation allocation and replacement are one transaction.
    /// The spawn closure must not await; completion cannot enter the registry
    /// until its own handle has been installed.
    pub(super) async fn replace(&self, uri: Url, spawn: impl FnOnce(u64) -> JoinHandle<()>) {
        let previous = {
            let mut registry = self.registry.lock().await;
            if registry.closed || *self.fatal.borrow() {
                return;
            }
            let Some(generation) = registry.next_generation.checked_add(1) else {
                panic!("diagnostics generation exhausted");
            };
            registry.next_generation = generation;
            let handle = spawn(generation);
            registry.tasks.insert(uri, (generation, handle))
        };
        if let Some((_, previous)) = previous {
            previous.abort();
            self.observe_task_result(previous.await);
        }
    }

    pub(super) async fn finish(&self, uri: &Url, generation: u64) {
        let mut registry = self.registry.lock().await;
        if registry
            .tasks
            .get(uri)
            .is_some_and(|(current, _)| *current == generation)
        {
            registry.tasks.remove(uri);
        }
    }

    pub(super) async fn cancel_if(&self, uri: &Url, is_current: impl FnOnce() -> bool) {
        let previous = {
            let mut registry = self.registry.lock().await;
            if !is_current() {
                return;
            }
            registry.tasks.remove(uri)
        };
        if let Some((_, previous)) = previous {
            previous.abort();
            self.observe_task_result(previous.await);
        }
    }

    pub(super) async fn shutdown(&self) {
        let tasks = {
            let mut registry = self.registry.lock().await;
            registry.closed = true;
            std::mem::take(&mut registry.tasks)
        };
        for (_, task) in tasks.values() {
            task.abort();
        }
        for (_, task) in tasks.into_values() {
            self.observe_task_result(task.await);
        }
    }
}
pub(super) fn diagnostics_revision_is_current(current: Option<Revision>, result: Revision) -> bool {
    current == Some(result)
}
#[cfg(test)]
pub(super) async fn publish_if_current<T, F, Fut>(
    current_revision: Option<Revision>,
    result_revision: Revision,
    result: T,
    publish: F,
) -> bool
where
    F: FnOnce(T) -> Fut,
    Fut: Future<Output = ()>,
{
    if !diagnostics_revision_is_current(current_revision, result_revision) {
        return false;
    }
    publish(result).await;
    true
}
pub(super) fn replace_imported_diagnostics_if_current(
    snapshots: &mut HashMap<Url, ImportedDiagnostics>,
    root_uri: Url,
    version: i32,
    by_uri: HashMap<Url, Vec<Diagnostic>>,
) -> Option<HashSet<Url>> {
    if snapshots
        .get(&root_uri)
        .is_some_and(|previous| previous.version.is_some_and(|previous| previous > version))
    {
        return None;
    }
    let mut affected = HashSet::new();
    if let Some(previous) = snapshots.get(&root_uri) {
        affected.extend(previous.by_uri.keys().cloned());
        affected.extend(previous.retired.iter().cloned());
    }
    affected.extend(by_uri.keys().cloned());
    snapshots.insert(
        root_uri,
        ImportedDiagnostics {
            version: Some(version),
            by_uri,
            retired: HashSet::new(),
            context: None,
        },
    );
    Some(affected)
}

impl Backend {
    pub(super) async fn schedule_diagnostics(&self, uri: Url, version: i32) {
        let backend = self.clone();
        let task_uri = uri.clone();
        let span = tracing::info_span!(
            parent: None, "diagnostics.run", revision = version,
            diagnostic_count = tracing::field::Empty, outcome = tracing::field::Empty,
        );
        self.diagnostics
            .replace(uri, move |generation| {
                tokio::spawn(
                    async move {
                        let operation = async {
                            backend.run_diagnostics(task_uri.clone(), version).await;
                            backend.diagnostics.finish(&task_uri, generation).await;
                        };
                        super::state::supervise(operation, || backend.diagnostics.report_fatal())
                            .await;
                    }
                    .instrument(span),
                )
            })
            .await;
    }
    pub(super) async fn cancel_diagnostics(
        &self,
        uri: &Url,
        closed: super::workspace_service::ClosedRevision,
    ) {
        self.diagnostics
            .cancel_if(uri, || self.workspace.is_closed(closed))
            .await;
    }
    pub(super) async fn cancel_all_diagnostics(&self) {
        self.shutdown_lint().await;
        self.compiler.reapers.close();
        self.compiler.semaphore.close();
        self.diagnostics.shutdown().await;
        self.clear_coordinated_diagnostics().await;
        self.workspace.shutdown_discovery().await;
        self.compiler.reapers.wait().await;
    }
    pub(super) async fn run_diagnostics(&self, uri: Url, version: i32) {
        let span = tracing::Span::current();
        sleep(Duration::from_millis(250)).await;
        let Some(doc) = self.document(&uri) else {
            span.record("outcome", "document_missing");
            return;
        };
        if doc.revision.0 != version || !Self::supported(&doc) {
            span.record("outcome", "stale_or_unsupported");
            return;
        }
        let context_epoch = self.diagnostics.context_epoch.load(Ordering::SeqCst);
        let mut context = None;
        let Some(mut diagnostics) = self.lexical_errors(doc.clone(), span.clone()).await else {
            span.record("outcome", "unavailable");
            return;
        };

        if let Some((path, source_graph)) = self.compiler_input_graph(&uri, &span).await {
            let root_matches = source_graph
                .root_node()
                .and_then(|root| root.snapshot.as_ref())
                .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &doc.snapshot));
            if !root_matches {
                span.record("outcome", "stale_snapshot");
                return;
            }
            let Some(captured_context) =
                self.capture_diagnostics_context(context_epoch, &source_graph, &uri)
            else {
                return;
            };
            let compiler = self.compiler_errors(&doc, &path, source_graph).await;
            let mut imported = HashMap::<Url, Vec<Diagnostic>>::new();
            if !diagnostics
                .iter()
                .any(|item| item.code == Some(NumberOrString::String("holes".into())))
            {
                let current_revision = self
                    .document(&uri)
                    .filter(|current| Arc::ptr_eq(&current.snapshot, &doc.snapshot))
                    .map(|current| current.revision);
                if !diagnostics_revision_is_current(current_revision, Revision(version)) {
                    span.record("outcome", "stale_revision");
                    return;
                }
                let root_path = normalize_path(&path);
                for (source_path, item) in compiler {
                    if normalize_path(&source_path) == root_path {
                        diagnostics.push(item);
                    } else if let Ok(source_uri) = Url::from_file_path(&source_path)
                        && self.document(&source_uri).is_none()
                    {
                        imported.entry(source_uri).or_default().push(item);
                    }
                }
            }
            self.replace_import_diagnostics(
                uri.clone(),
                version,
                imported,
                &doc.snapshot,
                &captured_context,
            )
            .await;
            context = Some(captured_context);
        }
        let context = context.unwrap_or_else(|| {
            Arc::new(DiagnosticsContext {
                epoch: context_epoch,
                root: None,
                sources: Vec::new(),
            })
        });
        let _publish = self.diagnostics.imported_publish.lock().await;
        if !self.diagnostics_context_current(&context)
            || !self.diagnostics_snapshot_current(&uri, &doc.snapshot)
        {
            span.record("outcome", "stale");
            return;
        }
        span.record("diagnostic_count", diagnostics.len());
        self.diagnostics.documents.write().insert(
            uri.clone(),
            DocumentDiagnostics {
                snapshot: doc.snapshot.clone(),
                diagnostics,
                context,
            },
        );
        self.publish_coordinated_locked(HashSet::from([uri])).await;
        span.record("outcome", "published");
    }

    async fn compiler_input_graph(
        &self,
        uri: &Url,
        span: &tracing::Span,
    ) -> Option<(PathBuf, SourceGraph)> {
        let path = self.document_path(uri)?;
        if let Some(graph) = self.compiler_source_graph(uri.clone()).await {
            return Some((path, graph));
        }
        // Conflicting open views are a workspace ambiguity, not a source error.
        if let Some(reason) = self.compiler_ambiguity(uri).await {
            span.record("outcome", "ambiguous_views");
            self.client.log_message(MessageType::WARNING, reason).await;
        }
        None
    }

    // Only invoked after a source graph failed closed, so an ordinary missing
    // or compiler-owned source never produces an ambiguity message.
    async fn compiler_ambiguity(&self, uri: &Url) -> Option<String> {
        let workspace = self.workspace.clone();
        let uri = uri.clone();
        super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                let database = workspace.read();
                let root = database.file_id_by_uri(&uri)?;
                database.source_view_ambiguity(root)
            })
            .await,
        )
    }

    async fn lexical_errors(
        &self,
        document: Document,
        span: tracing::Span,
    ) -> Option<Vec<Diagnostic>> {
        let permit = self
            .diagnostics
            .analysis
            .clone()
            .acquire_owned()
            .await
            .ok()?;
        Some(super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                span.in_scope(|| {
                    let _permit = permit;
                    lexical_diagnostics(&document)
                })
            })
            .await,
        ))
    }

    async fn compiler_errors(
        &self,
        doc: &Document,
        path: &Path,
        source_graph: SourceGraph,
    ) -> Vec<(PathBuf, Diagnostic)> {
        let config = self.compiler.config.read().clone();
        let compatibility = self.compiler.compatibility(&config).await;
        if let Some((message, code)) = compatibility.failure() {
            vec![(
                path.to_path_buf(),
                super::features::diag(&doc.snapshot, 0, 0, message, code),
            )]
        } else {
            compiler_diagnostics(
                source_graph,
                config,
                compatibility.backend(),
                self.compiler.semaphore.clone(),
                self.compiler.results.clone(),
                self.compiler.reapers.clone(),
            )
            .await
        }
    }
    async fn replace_import_diagnostics(
        &self,
        root_uri: Url,
        version: i32,
        by_uri: HashMap<Url, Vec<Diagnostic>>,
        snapshot: &Arc<crate::analysis::DocumentSnapshot>,
        context: &Arc<DiagnosticsContext>,
    ) {
        let _view = self.workspace.updates.read().await;
        if !self.diagnostics_context_current(context) {
            return;
        }
        let affected = {
            let state = self.workspace.state.read();
            let Some(status) = state.revision_status(&root_uri) else {
                return;
            };
            if status.desired() != Some(Revision(version))
                || state
                    .database
                    .open_document(&root_uri)
                    .is_none_or(|document| !Arc::ptr_eq(&document.snapshot, snapshot))
            {
                return;
            }
            let mut snapshots = self.diagnostics.imported.write();
            let Some(affected) = replace_imported_diagnostics_if_current(
                &mut snapshots,
                root_uri.clone(),
                version,
                by_uri,
            ) else {
                return;
            };
            if let Some(snapshot) = snapshots.get_mut(&root_uri) {
                snapshot.context = Some(context.clone());
            }
            affected
        };
        self.publish_import_diagnostics(affected).await;
    }
    /// Detach the closed epoch and publish current aggregates before close can
    /// be superseded by a reopened document that skips compiler diagnostics.
    pub(super) async fn detach_import_diagnostics(
        &self,
        root_uri: &Url,
        closed: super::workspace_service::ClosedRevision,
    ) {
        let affected = {
            let state = self.workspace.state.read();
            if !state.is_closed(closed) {
                return;
            }
            let mut snapshots = self.diagnostics.imported.write();
            let Some(snapshot) = snapshots.get_mut(root_uri) else {
                return;
            };
            let affected = snapshot.by_uri.keys().cloned().collect();
            snapshot.version = None;
            snapshot
                .retired
                .extend(std::mem::take(&mut snapshot.by_uri).into_keys());
            affected
        };
        self.publish_import_diagnostics(affected).await;
    }

    pub(super) async fn remove_import_diagnostics(&self, root_uri: &Url) {
        let affected = {
            let mut snapshots = self.diagnostics.imported.write();
            let Some(previous) = snapshots.remove(root_uri) else {
                return;
            };
            previous
                .retired
                .into_iter()
                .chain(previous.by_uri.into_keys())
                .collect()
        };
        self.publish_import_diagnostics(affected).await;
    }

    fn capture_diagnostics_context(
        &self,
        epoch: u64,
        graph: &SourceGraph,
        root_uri: &Url,
    ) -> Option<Arc<DiagnosticsContext>> {
        let database = self.workspace.read();
        if graph
            .nodes
            .iter()
            .any(|node| database.file_id_by_path(&node.path) != Some(node.id))
        {
            return None;
        }
        // The root's analyzed text is the requested spelling's own buffer, not
        // whatever alias currently supplies the representative text. Capture
        // that spelling as identity so publication can validate it directly.
        let root_snapshot = graph.root_node().and_then(|node| node.snapshot.as_ref())?;
        if !database
            .open_document(root_uri)
            .is_some_and(|document| Arc::ptr_eq(root_snapshot, &document.snapshot))
        {
            return None;
        }
        Some(Arc::new(DiagnosticsContext {
            epoch,
            root: Some(RootView {
                id: graph.root,
                uri: root_uri.clone(),
            }),
            sources: graph
                .nodes
                .iter()
                .map(|node| SourceIdentity {
                    id: node.id,
                    snapshot: node.snapshot.clone(),
                    disk_generation: database.disk_generation(node.id),
                })
                .collect(),
        }))
    }

    fn diagnostics_context_current(&self, context: &DiagnosticsContext) -> bool {
        self.diagnostics_context_current_in(context, &self.workspace.state.read())
    }

    fn diagnostics_context_current_in(
        &self,
        context: &DiagnosticsContext,
        state: &super::workspace_service::WorkspaceState,
    ) -> bool {
        if self.diagnostics.context_epoch.load(Ordering::SeqCst) != context.epoch {
            return false;
        }
        context.sources.iter().all(|source| {
            let expected_revision = source
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.revision)
                .filter(|revision| *revision != Revision::UNVERSIONED);
            let root_view = context.root.as_ref().filter(|view| view.id == source.id);
            let current = if let Some(root) = root_view {
                // The root publishes the requested spelling's own buffer and
                // version stream; another alias legitimately supplies the
                // representative text and revision for dependencies only.
                let open = state.database.open_document(&root.uri);
                let same = open.as_ref().is_some_and(|doc| {
                    source
                        .snapshot
                        .as_ref()
                        .is_some_and(|s| Arc::ptr_eq(s, &doc.snapshot))
                });
                let revision = state.revision_status(&root.uri);
                same && revision.is_some_and(|s| s.desired() == expected_revision)
            } else {
                let same = match (&source.snapshot, state.database.source_snapshot(source.id)) {
                    (Some(expected), Some(actual)) => Arc::ptr_eq(expected, actual),
                    (None, None) => true,
                    _ => false,
                };
                let revision = state.file_revision_status(source.id);
                same && revision.is_none_or(|s| s.desired() == expected_revision)
            };
            current && state.database.disk_generation(source.id) == source.disk_generation
        })
    }
    pub(super) fn diagnostics_snapshot_current(
        &self,
        uri: &Url,
        snapshot: &Arc<crate::analysis::DocumentSnapshot>,
    ) -> bool {
        let state = self.workspace.state.read();
        let Some(status) = state.revision_status(uri) else {
            return false;
        };
        status.desired() == Some(snapshot.revision)
            && state
                .database
                .open_document(uri)
                .is_some_and(|document| Arc::ptr_eq(&document.snapshot, snapshot))
    }

    pub(super) async fn publish_import_diagnostics(&self, affected: HashSet<Url>) {
        self.publish_coordinated_diagnostics(affected).await;
    }

    async fn clear_coordinated_diagnostics(&self) {
        let _publish = self.diagnostics.imported_publish.lock().await;
        let mut affected: HashSet<_> = self
            .diagnostics
            .documents
            .write()
            .drain()
            .map(|(uri, _)| uri)
            .collect();
        for snapshot in std::mem::take(&mut *self.diagnostics.imported.write()).into_values() {
            affected.extend(snapshot.by_uri.into_keys());
            affected.extend(snapshot.retired);
        }
        self.publish_coordinated_locked(affected).await;
    }

    pub(super) async fn publish_coordinated_diagnostics(&self, affected: HashSet<Url>) {
        let _publish = self.diagnostics.imported_publish.lock().await;
        self.publish_coordinated_locked(affected).await;
    }

    // Every provider replaces only its own contribution. Publication is one
    // serialized transaction, so a late compiler packet cannot erase lint and
    // clearing lint never erases compiler/imported diagnostics.
    pub(super) async fn publish_coordinated_locked(&self, affected: HashSet<Url>) {
        for uri in affected {
            let document = self.document(&uri);
            let mut combined = Vec::new();
            if let Some(document) = &document {
                if let Some(result) = self.diagnostics.documents.read().get(&uri)
                    && Arc::ptr_eq(&result.snapshot, &document.snapshot)
                    && self.diagnostics_snapshot_current(&uri, &result.snapshot)
                    && self.diagnostics_context_current(&result.context)
                {
                    combined.extend(result.diagnostics.iter().cloned());
                }
            } else {
                let state = self.workspace.state.read();
                for snapshot in self.diagnostics.imported.read().values() {
                    if snapshot
                        .context
                        .as_ref()
                        .is_none_or(|context| self.diagnostics_context_current_in(context, &state))
                        && let Some(items) = snapshot.by_uri.get(&uri)
                    {
                        for item in items {
                            if !combined.contains(item) {
                                combined.push(item.clone());
                            }
                        }
                    }
                }
            }
            combined.extend(self.lint_diagnostics(&uri));
            let version = document.as_ref().map(|document| document.revision.0);
            let kind = if document.is_some() {
                "document"
            } else {
                "imported_target"
            };
            let publish_span = tracing::info_span!(
                "diagnostics.publish",
                kind,
                diagnostic_count = combined.len(),
                revision = version,
                outcome = tracing::field::Empty,
            );
            self.client
                .publish_diagnostics(uri, combined, version)
                .instrument(publish_span.clone())
                .await;
            publish_span.record("outcome", "published");
        }
    }
}

#[cfg(test)]
mod service_tests {
    use super::DiagnosticsService;
    use std::{io, time::Duration};
    use tokio::sync::oneshot;
    use url::Url;

    struct Dropped(Option<oneshot::Sender<()>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn obsolete_completion_and_close_cannot_remove_replacement() -> io::Result<()> {
        let service = DiagnosticsService::default();
        let uri = Url::parse("file:///diagnostics-generation.bend").map_err(io::Error::other)?;
        let mut first_generation = 0;
        service
            .replace(uri.clone(), |generation| {
                first_generation = generation;
                tokio::spawn(std::future::pending())
            })
            .await;
        let (started, running) = oneshot::channel();
        let (dropped, cancelled) = oneshot::channel();
        let (probe, requested) = oneshot::channel();
        let (probed, survived) = oneshot::channel();
        service
            .replace(uri.clone(), |_| {
                tokio::spawn(async move {
                    let _drop = Dropped(Some(dropped));
                    let _ = started.send(());
                    if requested.await.is_ok() {
                        let _ = probed.send(());
                    }
                    std::future::pending::<()>().await;
                })
            })
            .await;
        tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        service.finish(&uri, first_generation).await;
        service.cancel_if(&uri, || false).await;
        probe
            .send(())
            .map_err(|()| io::Error::other("replacement cancelled by obsolete action"))?;
        tokio::time::timeout(Duration::from_secs(1), survived)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        assert!(!*service.fatal_receiver().borrow());
        service.cancel_if(&uri, || true).await;
        tokio::time::timeout(Duration::from_secs(1), cancelled)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        service.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_drains_running_tasks_and_rejects_new_admission() -> io::Result<()> {
        let service = DiagnosticsService::default();
        let uri = Url::parse("file:///diagnostics-shutdown.bend").map_err(io::Error::other)?;
        let (started, running) = oneshot::channel();
        let (dropped, cancelled) = oneshot::channel();
        service
            .replace(uri.clone(), |_| {
                tokio::spawn(async move {
                    let _drop = Dropped(Some(dropped));
                    let _ = started.send(());
                    std::future::pending::<()>().await;
                })
            })
            .await;
        tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .map_err(io::Error::other)?
            .map_err(io::Error::other)?;
        service.shutdown().await;
        cancelled.await.map_err(io::Error::other)?;
        service
            .replace(uri, |_| panic!("shutdown must reject admission"))
            .await;
        assert!(!*service.fatal_receiver().borrow());
        Ok(())
    }

    #[tokio::test]
    async fn failed_task_marks_service_fatal_and_rejects_admission() -> io::Result<()> {
        let service = DiagnosticsService::default();
        let uri = Url::parse("file:///diagnostics-fatal.bend").map_err(io::Error::other)?;
        let (failed, observed) = oneshot::channel();
        service
            .replace(uri.clone(), |_| {
                tokio::spawn(async move {
                    let _drop = Dropped(Some(failed));
                    panic!("diagnostics invariant");
                })
            })
            .await;
        observed.await.map_err(io::Error::other)?;
        service.shutdown().await;
        assert!(*service.fatal_receiver().borrow());
        service
            .replace(uri, |_| panic!("fatal service must reject admission"))
            .await;
        Ok(())
    }

    #[tokio::test]
    async fn fatal_signal_rejects_admission_before_shutdown() -> io::Result<()> {
        let service = DiagnosticsService::default();
        let uri =
            Url::parse("file:///diagnostics-fatal-admission.bend").map_err(io::Error::other)?;
        service.report_fatal();
        service
            .replace(uri, |_| panic!("fatal signal must reject admission"))
            .await;
        service.shutdown().await;
        Ok(())
    }
}
