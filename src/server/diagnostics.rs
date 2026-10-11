use super::{
    compiler::compiler_diagnostics, features::lexical_diagnostics, lsp::Backend, state::State,
};
use crate::{
    analysis::Revision,
    workspace::{Document, SourceGraph, normalize_path},
};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{Mutex, Semaphore, watch},
    task::{JoinError, JoinHandle},
    time::sleep,
};
use tower_lsp::lsp_types::{Diagnostic, NumberOrString};
use tracing::Instrument;
use url::Url;

#[derive(Clone)]
pub(super) struct ImportedDiagnostics {
    version: Option<i32>,
    pub(super) by_uri: HashMap<Url, Vec<Diagnostic>>,
    retired: HashSet<Url>,
}

#[derive(Default)]
struct TaskRegistry {
    closed: bool,
    next_generation: u64,
    tasks: HashMap<Url, (u64, JoinHandle<()>)>,
}

pub(super) struct DiagnosticsService {
    registry: Mutex<TaskRegistry>,
    fatal: watch::Sender<bool>,
    pub(super) imported: State<HashMap<Url, ImportedDiagnostics>>,
    pub(super) imported_publish: Mutex<()>,
    pub(super) analysis: Arc<Semaphore>,
}

impl Default for DiagnosticsService {
    fn default() -> Self {
        Self {
            registry: Mutex::new(TaskRegistry::default()),
            fatal: watch::channel(false).0,
            imported: State::new(HashMap::new()),
            imported_publish: Mutex::new(()),
            analysis: Arc::new(Semaphore::new(4)),
        }
    }
}

impl DiagnosticsService {
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
        self.compiler.reapers.close();
        self.compiler.semaphore.close();
        self.diagnostics.shutdown().await;
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
        let lexical_document = doc.clone();
        let Ok(permit) = self.diagnostics.analysis.clone().acquire_owned().await else {
            span.record("outcome", "unavailable");
            return;
        };
        let analysis_span = span.clone();
        let mut diagnostics = super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                analysis_span.in_scope(|| {
                    let _permit = permit;
                    lexical_diagnostics(&lexical_document)
                })
            })
            .await,
        );

        if let Some(path) = self.document_path(&uri)
            && let Some(source_graph) = self.compiler_source_graph(uri.clone()).await
        {
            let root_matches = source_graph
                .root_node()
                .and_then(|root| root.snapshot.as_ref())
                .is_some_and(|snapshot| Arc::ptr_eq(snapshot, &doc.snapshot));
            if !root_matches {
                span.record("outcome", "stale_snapshot");
                return;
            }
            let compiler = self.compiler_errors(&doc, &path, source_graph).await;
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
                let mut imported = HashMap::<Url, Vec<Diagnostic>>::new();
                for (source_path, item) in compiler {
                    if normalize_path(&source_path) == root_path {
                        diagnostics.push(item);
                    } else if let Ok(source_uri) = Url::from_file_path(&source_path)
                        && self.document(&source_uri).is_none()
                    {
                        imported.entry(source_uri).or_default().push(item);
                    }
                }
                self.replace_import_diagnostics(uri.clone(), version, imported, &doc.snapshot)
                    .await;
            }
        }
        let current_revision = self
            .document(&uri)
            .filter(|current| Arc::ptr_eq(&current.snapshot, &doc.snapshot))
            .map(|current| current.revision);
        span.record("diagnostic_count", diagnostics.len());
        let published = publish_if_current(
            current_revision,
            Revision(version),
            diagnostics,
            |diagnostics| async move {
                let publish_span = tracing::info_span!(
                    "diagnostics.publish",
                    kind = "document",
                    revision = version,
                    diagnostic_count = diagnostics.len(),
                    outcome = tracing::field::Empty,
                );
                self.client
                    .publish_diagnostics(uri, diagnostics, Some(version))
                    .instrument(publish_span.clone())
                    .await;
                publish_span.record("outcome", "published");
            },
        )
        .await;
        span.record("outcome", if published { "published" } else { "stale" });
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
                self.compiler.semaphore.clone(),
                self.compiler.results.clone(),
                self.compiler.reapers.clone(),
            )
            .await
        }
    }
    pub(super) async fn replace_import_diagnostics(
        &self,
        root_uri: Url,
        version: i32,
        by_uri: HashMap<Url, Vec<Diagnostic>>,
        snapshot: &Arc<crate::analysis::DocumentSnapshot>,
    ) {
        let affected = {
            let state = self.workspace.state.read();
            let Some(file) = state.database.file_id_by_uri(&root_uri) else {
                return;
            };
            let Some(sync) = state.revisions.get(&file) else {
                return;
            };
            if super::state::revision_result(sync.status()).desired() != Some(Revision(version))
                || state
                    .database
                    .open_document(&root_uri)
                    .is_none_or(|document| !Arc::ptr_eq(&document.snapshot, snapshot))
            {
                return;
            }
            let mut snapshots = self.diagnostics.imported.write();
            let Some(affected) =
                replace_imported_diagnostics_if_current(&mut snapshots, root_uri, version, by_uri)
            else {
                return;
            };
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
    #[tracing::instrument(name = "diagnostics.publish", skip_all, fields(kind = "imported", target_count = affected.len(), published_count = tracing::field::Empty))]
    pub(super) async fn publish_import_diagnostics(&self, affected: HashSet<Url>) {
        let _publish = self.diagnostics.imported_publish.lock().await;
        let span = tracing::Span::current();
        let mut published_count = 0;
        for uri in affected {
            if self.document(&uri).is_some() {
                continue;
            }
            let diagnostics = {
                let snapshots = self.diagnostics.imported.read();
                let mut combined = Vec::new();
                for snapshot in snapshots.values() {
                    if let Some(items) = snapshot.by_uri.get(&uri) {
                        for item in items {
                            if !combined.iter().any(|existing: &Diagnostic| {
                                existing.range == item.range
                                    && existing.message == item.message
                                    && existing.code == item.code
                            }) {
                                combined.push(item.clone());
                            }
                        }
                    }
                }
                combined
            };
            if self.document(&uri).is_none() {
                let publish_span = tracing::info_span!(
                    "diagnostics.publish",
                    kind = "imported_target",
                    diagnostic_count = diagnostics.len(),
                    outcome = tracing::field::Empty,
                );
                self.client
                    .publish_diagnostics(uri, diagnostics, None)
                    .instrument(publish_span.clone())
                    .await;
                publish_span.record("outcome", "published");
                published_count += 1;
            }
        }
        span.record("published_count", published_count);
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
