use super::{
    orchestration::{run_staging, trace_document_snapshot},
    state::{blocking_result, supervise},
    workspace_service::WorkspaceService,
};
use crate::{
    analysis::{DocumentSnapshot, Revision},
    workspace::{FileId, PreparedImportTarget, canonical_source_path, normalize_path},
};
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkState};
use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::Ordering, mpsc},
};

struct PreparedSource {
    path: PathBuf,
    physical_path: PathBuf,
    snapshot: Arc<DocumentSnapshot>,
    imports: Vec<PreparedImportTarget>,
    disk_generation: Option<(FileId, u64)>,
    accepted: bool,
}

pub(super) fn walk_builder(roots: &[PathBuf]) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::from_iter(roots);
    builder
        .follow_links(false)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .filter_entry(|entry| entry.depth() == 0 || discoverable_name(entry.file_name()));
    builder
}

/// Two walkers overlap independent cold file reads and snapshot construction.
/// Discovery competes with interactive requests for the same cores, so the
/// bound stays fixed and small under the existing staging admission.
const MAX_DISCOVERY_WORKERS: usize = 2;

fn discovery_worker_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(MAX_DISCOVERY_WORKERS)
}

/// Builds one visitor per walker thread. Each visitor owns its chunk of
/// prepared sources and sends it once when the walker joins the thread, so the
/// walk itself performs no shared-state locking.
struct DiscoveryVisitorBuilder<'a> {
    workspace: &'a WorkspaceService,
    epoch: u64,
    prepared: mpsc::Sender<Vec<PreparedSource>>,
    span: tracing::Span,
}

impl<'s> ParallelVisitorBuilder<'s> for DiscoveryVisitorBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(DiscoveryVisitor {
            workspace: self.workspace,
            epoch: self.epoch,
            prepared: self.prepared.clone(),
            sources: Vec::new(),
            span: self.span.clone(),
        })
    }
}

struct DiscoveryVisitor<'a> {
    workspace: &'a WorkspaceService,
    epoch: u64,
    prepared: mpsc::Sender<Vec<PreparedSource>>,
    sources: Vec<PreparedSource>,
    span: tracing::Span,
}

impl ParallelVisitor for DiscoveryVisitor<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        // Quit is observed by every walker thread, so cancellation stops
        // admission promptly; WalkParallel joins its owned threads before
        // returning, and nothing is committed for a stale epoch.
        if !self.workspace.discovery_is_current(self.epoch) {
            return WalkState::Quit;
        }
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            return WalkState::Continue;
        }
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "bend") {
            return WalkState::Continue;
        }
        let _span = self.span.enter();
        if let Some(source) = self.workspace.prepare_source(path) {
            self.sources.push(source);
        }
        WalkState::Continue
    }
}

impl Drop for DiscoveryVisitor<'_> {
    fn drop(&mut self) {
        if !self.sources.is_empty() {
            // The receiver outlives the walk; drop happens before it returns.
            let _ = self.prepared.send(std::mem::take(&mut self.sources));
        }
    }
}

fn discoverable_name(name: &std::ffi::OsStr) -> bool {
    !name.as_encoded_bytes().starts_with(b".") && name != "target" && name != "node_modules"
}

pub(super) fn allowed_path(path: &Path, roots: &[PathBuf]) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "bend")
        && roots.iter().any(|root| {
            path.strip_prefix(root).is_ok_and(|relative| {
                relative.components().all(|component| match component {
                    Component::Normal(name) => discoverable_name(name),
                    _ => false,
                })
            })
        })
}

impl WorkspaceService {
    fn discovery_is_current(&self, epoch: u64) -> bool {
        !self.discovery_closed.load(Ordering::Acquire)
            && self.discovery_epoch.load(Ordering::Acquire) == epoch
    }

    pub(super) async fn start_discovery(self: &Arc<Self>) {
        if self.discovery_closed.load(Ordering::Acquire) {
            return;
        }
        let epoch = self
            .discovery_epoch
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let roots = self.roots.read().clone();
        let mut task = self.discovery.lock().await;
        // Borrow the owned handle across await: cancellation of this caller must
        // not detach the worker or let another query observe an unfinished index.
        if let Some(previous) = task.as_mut() {
            blocking_result(previous.await);
        }
        task.take();
        if !self.discovery_is_current(epoch) {
            return;
        }
        let workspace = self.clone();
        *task = Some(tokio::spawn(async move {
            supervise(workspace.run_discovery(epoch, roots), || {
                workspace.discovery_failure.send_replace(true);
            })
            .await;
        }));
    }

    fn prepare_source(&self, path: &Path) -> Option<PreparedSource> {
        let path = normalize_path(path);
        // One read guard per source keeps the skip and generation capture
        // consistent without adding a lock per token or per byte of source.
        let (has_snapshot, disk_generation) = {
            let database = self.read();
            database.discovery_state(&path)
        };
        if has_snapshot {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let snapshot = Arc::new(trace_document_snapshot(Revision::UNVERSIONED, text));
        let physical_path = canonical_source_path(&path);
        let imports = crate::workspace::resolve_import_targets(&path, &physical_path, &snapshot);
        Some(PreparedSource {
            path,
            physical_path,
            snapshot,
            imports,
            disk_generation,
            accepted: false,
        })
    }

    fn prepare_discovery(&self, epoch: u64, roots: &[PathBuf]) -> Vec<PreparedSource> {
        if !self.discovery_is_current(epoch) {
            return Vec::new();
        }
        let (prepared, collected) = mpsc::channel();
        let mut visitors = DiscoveryVisitorBuilder {
            workspace: self,
            epoch,
            prepared,
            span: tracing::Span::current(),
        };
        // The owning staging task already holds one permit. A second walker
        // uses another existing permit, or stays serial when other work owns it.
        let extra_worker = (discovery_worker_count() > 1)
            .then(|| self.staging.clone().try_acquire_owned().ok())
            .flatten();
        let mut builder = walk_builder(roots);
        builder.threads(1 + usize::from(extra_worker.is_some()));
        // Blocks until every walker thread has been joined, so no discovery
        // worker survives cancellation or a stale epoch.
        builder.build_parallel().visit(&mut visitors);
        drop(extra_worker);
        drop(visitors);
        let mut sources = Vec::new();
        // Every visitor has been joined and dropped, so the channel is
        // disconnected and this drains each worker's chunk exactly once.
        for chunk in collected {
            sources.extend(chunk);
        }
        // Parallel traversal order is scheduling dependent; commit in a stable
        // path order so identities and diagnostics never depend on timing.
        sources.sort_unstable_by(|left, right| left.path.cmp(&right.path));
        sources
    }

    async fn run_discovery(self: &Arc<Self>, epoch: u64, roots: Vec<PathBuf>) {
        let workspace = self.clone();
        let Some((roots, mut prepared)) = run_staging(self.staging.clone(), move || {
            let sources = workspace.prepare_discovery(epoch, &roots);
            (roots, sources)
        })
        .await
        else {
            return;
        };
        let update = self.updates.write().await;
        if !self.discovery_is_current(epoch) || *self.roots.read() != roots {
            return;
        }
        self.commit(None, None, |database| {
            // Validate before this batch's import edges intern new identities.
            for source in &mut prepared {
                source.accepted =
                    database.discovery_state(&source.path).1 == source.disk_generation;
            }
            for source in prepared {
                if source.accepted {
                    database.discover_snapshot(
                        &source.path,
                        &source.physical_path,
                        source.snapshot,
                        source.imports,
                    );
                } else {
                    database.admit_discovered_path(source.path);
                }
            }
            Some(())
        });
        drop(update);
        let discovered = self.read().discovered_roots();
        self.load_reachable_async(&discovered, Some(epoch)).await;
    }

    pub(super) fn discovery_pending(&self) -> bool {
        // A query waiting on the owned worker can hold this mutex. Completion
        // must not join that wait merely to decide whether its list is partial.
        self.discovery.try_lock().map_or(true, |task| {
            task.as_ref().is_some_and(|handle| !handle.is_finished())
        })
    }

    pub(super) async fn wait_for_discovery_worker(&self) {
        let mut task = self.discovery.lock().await;
        if let Some(handle) = task.as_mut() {
            blocking_result(handle.await);
        }
        task.take();
    }

    pub(super) async fn wait_for_discovery(&self) {
        loop {
            self.wait_for_discovery_worker().await;
            let serial = self.update_serial.lock().await;
            if self.discovery.lock().await.is_none() {
                return;
            }
            drop(serial);
        }
    }

    pub(super) async fn cancel_discovery(&self) {
        self.discovery_epoch.fetch_add(1, Ordering::AcqRel);
        let mut task = self.discovery.lock().await;
        if let Some(handle) = task.as_mut() {
            blocking_result(handle.await);
        }
        task.take();
    }

    pub(super) async fn shutdown_discovery(&self) {
        self.discovery_closed.store(true, Ordering::Release);
        self.discovery_epoch.fetch_add(1, Ordering::AcqRel);
        let mut task = self.discovery.lock().await;
        if let Some(handle) = task.as_mut() {
            // The supervised task already reports a panic to the transport.
            let _ = handle.await;
        }
        task.take();
    }
    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "load_reachable", root_count = roots.len(), outcome = tracing::field::Empty))]
    pub(super) async fn load_reachable_async(
        self: &Arc<Self>,
        roots: &[FileId],
        discovery_epoch: Option<u64>,
    ) {
        let mut attempted = HashSet::new();
        loop {
            if discovery_epoch.is_some_and(|epoch| !self.discovery_is_current(epoch)) {
                break;
            }
            let paths = {
                let database = self.read();
                database
                    .missing_disk_paths(roots)
                    .into_iter()
                    .filter_map(|path| {
                        if attempted.contains(&path) {
                            return None;
                        }
                        let id = database.file_id_by_path(&path)?;
                        if !database.is_needed(id) {
                            return None;
                        }
                        Some((id, database.disk_generation(id)?, path))
                    })
                    .collect::<Vec<_>>()
            };
            if paths.is_empty() {
                break;
            }
            let discovery = discovery_epoch.map(|epoch| (self.clone(), epoch));
            let Some(prepared) = run_staging(self.staging.clone(), move || {
                paths
                    .into_iter()
                    .take_while(|_| {
                        discovery
                            .as_ref()
                            .is_none_or(|(workspace, epoch)| workspace.discovery_is_current(*epoch))
                    })
                    .map(|(id, generation, path)| {
                        let regular = discovery.is_none()
                            || std::fs::metadata(&path).is_ok_and(|metadata| metadata.is_file());
                        let snapshot = regular
                            .then(|| std::fs::read_to_string(&path).ok())
                            .flatten()
                            .map(|text| {
                                Arc::new(trace_document_snapshot(Revision::UNVERSIONED, text))
                            });
                        let physical_path = canonical_source_path(&path);
                        let imports = snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
                            crate::workspace::resolve_import_targets(
                                &path,
                                &physical_path,
                                snapshot,
                            )
                        });
                        (id, generation, path, physical_path, snapshot, imports)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            else {
                break;
            };
            let _workspace_update = self.updates.write().await;
            if discovery_epoch.is_some_and(|epoch| !self.discovery_is_current(epoch)) {
                break;
            }
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "load_reachable",
                file_count = prepared.len(),
                outcome = tracing::field::Empty,
            );
            commit_span.in_scope(|| {
                self.commit(None, None, |database| {
                    let mut committed = false;
                    for (id, generation, path, physical_path, snapshot, imports) in prepared {
                        if !database.is_needed(id)
                            || database.disk_generation(id) != Some(generation)
                        {
                            continue;
                        }
                        let missing = snapshot.is_none();
                        committed |= database
                            .sync_disk_snapshot_prepared(&path, &physical_path, snapshot, imports)
                            .is_some();
                        if missing {
                            attempted.insert(path);
                        }
                    }
                    committed.then_some(())
                })
            });
            commit_span.record("outcome", "committed");
        }
        self.finish_load_reachable(roots.len()).await;
        tracing::Span::current().record("outcome", "complete");
    }

    pub(super) async fn admit_watched_sources(&self, events: &[(url::Url, PathBuf)]) {
        let roots = self.roots.read().clone();
        let candidates = {
            let database = self.read();
            events
                .iter()
                .filter(|(_, path)| {
                    database
                        .file_id_by_path(path)
                        .is_none_or(|id| !database.is_discovered(id))
                        && allowed_path(path, &roots)
                })
                .map(|(_, path)| path.clone())
                .collect::<Vec<_>>()
        };
        let candidates = blocking_result(
            tokio::task::spawn_blocking(move || {
                let mut matchers = walk_builder(&roots).build_matchers();
                candidates
                    .into_iter()
                    .filter(|path| {
                        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
                            && matchers.iter_mut().any(|matcher| {
                                path.strip_prefix(matcher.root()).is_ok_and(|relative| {
                                    !matcher.matched(relative, false).is_ignore()
                                })
                            })
                    })
                    .collect::<Vec<_>>()
            })
            .await,
        );
        if !candidates.is_empty() {
            let _update = self.updates.write().await;
            self.commit(None, None, |database| {
                for path in candidates {
                    database.admit_discovered_path(path);
                }
                Some(())
            });
        }
    }
    async fn finish_load_reachable(&self, root_count: usize) {
        // Retire payloads even when closed roots or an empty path set required no
        // reads. Finalize the current graph rather than a captured root's graph.
        let _workspace_update = self.updates.write().await;
        let commit_span = tracing::info_span!(
            "workspace.commit",
            kind = "finish_load_reachable",
            root_count,
            outcome = tracing::field::Empty,
        );
        commit_span.in_scope(|| {
            self.commit(None, None, |database| {
                database.finish_load_reachable();
                Some(())
            })
        });
        commit_span.record("outcome", "committed");
    }
}
