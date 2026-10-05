use std::{collections::HashSet, fs, io, path::PathBuf, sync::Arc};

use crate::{
    analysis::{DocumentSnapshot, Revision, TextRange},
    workspace::{
        PreparedSemanticSnapshot, normalize_path, prepare_semantic_snapshot, resolve_import_targets,
    },
};
use ignore::WalkBuilder;
use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
};
use tower_lsp::lsp_types::MessageType;
use url::Url;

use super::{lsp::Backend, orchestration::run_staging};

pub(super) struct DiscoveredSource {
    pub(super) path: PathBuf,
    pub(super) semantics: PreparedSemanticSnapshot,
    pub(super) imports: Vec<(TextRange, PathBuf)>,
}

#[derive(Clone, Copy, Default)]
struct DiscoveryStatus {
    generation: u64,
    completed: u64,
    pending_updates: usize,
    closed: bool,
}

/// Background disk discovery has separate readiness from document revisions:
/// workspace-wide queries wait for it; unrelated document queries do not.
pub(super) struct DiscoveryService {
    status: watch::Sender<DiscoveryStatus>,
    jobs: Mutex<Vec<JoinHandle<()>>>,
}

pub(super) struct WorkspaceUpdate {
    service: Arc<DiscoveryService>,
}

impl Drop for WorkspaceUpdate {
    fn drop(&mut self) {
        self.service.status.send_modify(|status| {
            status.pending_updates -= 1;
        });
    }
}

impl Default for DiscoveryService {
    fn default() -> Self {
        let (status, _) = watch::channel(DiscoveryStatus::default());
        Self {
            status,
            jobs: Mutex::new(Vec::new()),
        }
    }
}

impl DiscoveryService {
    pub(super) fn track_update(self: &Arc<Self>) -> Option<WorkspaceUpdate> {
        let mut admitted = false;
        self.status.send_if_modified(|status| {
            if status.closed {
                return false;
            }
            let Some(next) = status.pending_updates.checked_add(1) else {
                panic!("workspace update admission counter exhausted");
            };
            status.pending_updates = next;
            admitted = true;
            true
        });
        admitted.then(|| WorkspaceUpdate {
            service: self.clone(),
        })
    }

    pub(super) async fn schedule(self: &Arc<Self>, backend: Backend) {
        let mut generation = None;
        self.status.send_if_modified(|status| {
            if status.closed {
                return false;
            }
            if let Some(next) = status.generation.checked_add(1) {
                status.generation = next;
                generation = Some(next);
                true
            } else {
                panic!("workspace discovery generation exhausted");
            }
        });
        let Some(generation) = generation else {
            return;
        };
        let mut jobs = self.jobs.lock().await;
        if !self.is_current(generation) {
            return;
        }
        jobs.retain(|job| !job.is_finished());
        let service = self.clone();
        jobs.push(tokio::spawn(async move {
            let operation = async {
                if let Err(error) = service.discover(&backend, generation).await {
                    tracing::error!(%error, "workspace discovery failed");
                    backend
                        .client
                        .log_message(
                            MessageType::ERROR,
                            format!("Workspace indexing failed: {error}"),
                        )
                        .await;
                }
                service.status.send_if_modified(|status| {
                    if status.generation != generation {
                        return false;
                    }
                    status.completed = generation;
                    true
                });
            };
            super::state::supervise(operation, || backend.diagnostics.report_fatal()).await;
        }));
    }

    pub(super) async fn wait(&self) {
        let mut receiver = self.status.subscribe();
        loop {
            let state = *receiver.borrow_and_update();
            if state.closed || (state.completed == state.generation && state.pending_updates == 0) {
                return;
            }
            if receiver.changed().await.is_err() {
                return;
            }
        }
    }

    pub(super) async fn shutdown(&self) {
        self.status.send_modify(|status| status.closed = true);
        let jobs = std::mem::take(&mut *self.jobs.lock().await);
        for job in jobs {
            if let Err(error) = job.await {
                tracing::error!(%error, "workspace discovery task failed");
            }
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        let status = *self.status.borrow();
        !status.closed && status.generation == generation
    }

    async fn discover(self: &Arc<Self>, backend: &Backend, generation: u64) -> io::Result<()> {
        loop {
            if !self.is_current(generation) {
                return Ok(());
            }
            let revision = backend.workspace.generation();
            let roots = backend.workspace.roots.read().clone();
            let scanner = self.clone();
            let workspace = backend.workspace.clone();
            let Some(prepared) = run_staging(backend.workspace.staging.clone(), move || {
                let known_paths = workspace.read().disk_paths_in_roots(&roots);
                let sources = discover_sources(&roots, || scanner.is_current(generation))?;
                let scanned_paths: HashSet<_> =
                    sources.iter().map(|source| source.path.as_path()).collect();
                let mut deleted = Vec::new();
                for path in known_paths {
                    if !scanner.is_current(generation) {
                        break;
                    }
                    if scanned_paths.contains(path.as_path()) {
                        continue;
                    }
                    // Ignore rules and symlink exclusions can omit existing imported
                    // files. Only an explicit NotFound proves disk disappearance;
                    // other probe failures leave the cached snapshot untouched.
                    if fs::symlink_metadata(&path)
                        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
                    {
                        deleted.push(path);
                    }
                }
                Ok::<_, io::Error>((sources, deleted))
            })
            .await
            else {
                return Err(io::Error::other("workspace discovery staging failed"));
            };
            let (sources, deleted) = prepared?;
            if !self.is_current(generation) {
                return Ok(());
            }
            let _view = backend.workspace.updates.write().await;
            let applied = backend.workspace.commit(None, Some(revision), |database| {
                let mut active = HashSet::new();
                for source in sources {
                    let (id, _) = database.sync_disk_snapshot_prepared(
                        &source.path,
                        Some(source.semantics),
                        source.imports,
                    )?;
                    database.set_discovered_root(id, true);
                    active.insert(id);
                }
                for path in deleted {
                    database.sync_disk_snapshot_prepared(&path, None, Vec::new())?;
                }
                database.retain_discovered_roots(&active);
                Some(())
            });
            if applied.is_some() {
                return Ok(());
            }
        }
    }
}

/// Cold discovery honors workspace ignore files, skips hidden/build directories,
/// and never follows symlinks. Queries use the resulting immutable indexes.
fn discover_sources(
    roots: &[PathBuf],
    current: impl Fn() -> bool,
) -> io::Result<Vec<DiscoveredSource>> {
    let mut visited = HashSet::new();
    let mut sources = Vec::new();
    for root in roots {
        if !current() {
            break;
        }
        if !root.is_dir() {
            continue;
        }
        let walker = WalkBuilder::new(root)
            .follow_links(false)
            .parents(false)
            .git_global(false)
            .require_git(false)
            .filter_entry(|entry| {
                !entry.file_type().is_some_and(|kind| kind.is_dir())
                    || !matches!(entry.file_name().to_str(), Some("target" | "node_modules"))
            })
            .build();
        for entry in walker {
            if !current() {
                return Ok(sources);
            }
            let entry = entry.map_err(io::Error::other)?;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = normalize_path(entry.path());
            if path.extension().is_none_or(|extension| extension != "bend")
                || !visited.insert(path.clone())
            {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            Url::from_file_path(&path).map_err(|()| {
                io::Error::new(io::ErrorKind::InvalidInput, "workspace file has no URI")
            })?;
            let snapshot = Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, text));
            let imports = resolve_import_targets(&path, &snapshot);
            let semantics = prepare_semantic_snapshot(snapshot);
            sources.push(DiscoveredSource {
                path,
                semantics,
                imports,
            });
        }
    }
    sources.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(sources)
}
