use super::{
    revision::{DocumentRevisionSync, RevisionTicket},
    state::{State, revision_result},
};
use crate::{
    analysis::{DocumentSnapshot, Revision},
    workspace::{FileId, SemanticBase, SemanticView, WorkspaceDb},
};
use std::{
    collections::HashMap,
    ops::Deref,
    path::PathBuf,
    sync::{
        Arc, RwLockReadGuard,
        atomic::{AtomicBool, AtomicU64},
    },
};
use tokio::sync::{Mutex, RwLock, RwLockReadGuard as AsyncReadGuard, RwLockWriteGuard, Semaphore};
use url::Url;

pub(super) struct WorkspaceState {
    pub(super) database: WorkspaceDb,
    pub(super) revisions: HashMap<FileId, Arc<DocumentRevisionSync>>,
    generation: u64,
}

#[derive(Clone, Copy)]
pub(super) struct ClosedRevision {
    file: FileId,
    generation: u64,
}

impl WorkspaceState {
    pub(super) fn is_closed(&self, closed: ClosedRevision) -> bool {
        self.revisions.get(&closed.file).is_some_and(|sync| {
            let status = revision_result(sync.status());
            status.generation() == closed.generation && status.desired().is_none()
        })
    }

    fn apply<T>(&mut self, operation: impl FnOnce(&mut WorkspaceDb) -> Option<T>) -> Option<T> {
        let Some(next_generation) = self.generation.checked_add(1) else {
            panic!("workspace generation exhausted");
        };
        let result = operation(&mut self.database)?;
        self.generation = next_generation;
        Some(result)
    }
}

pub(super) struct WorkspaceRead<'a>(RwLockReadGuard<'a, WorkspaceState>);
impl Deref for WorkspaceRead<'_> {
    type Target = WorkspaceDb;
    fn deref(&self) -> &Self::Target {
        &self.0.database
    }
}

pub(super) enum WorkspaceOperationGuard<'a> {
    Concurrent { _guard: AsyncReadGuard<'a, ()> },
    Ordered { _guard: RwLockWriteGuard<'a, ()> },
}

pub(super) struct WorkspaceService {
    pub(super) state: State<WorkspaceState>,
    pub(super) update_serial: Mutex<()>,
    pub(super) updates: RwLock<()>,
    pub(super) file_operations: RwLock<()>,
    pub(super) roots: State<Vec<PathBuf>>,
    pub(super) staging: Arc<Semaphore>,
    pub(super) semantic_preparation: Mutex<()>,
    pub(super) state_changed: tokio::sync::Notify,
    pub(super) discovery: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) discovery_epoch: AtomicU64,
    pub(super) discovery_closed: AtomicBool,
    pub(super) discovery_failure: tokio::sync::watch::Sender<bool>,
}
impl Default for WorkspaceService {
    fn default() -> Self {
        Self {
            state: State::new(WorkspaceState {
                database: WorkspaceDb::default(),
                revisions: HashMap::new(),
                generation: 0,
            }),
            update_serial: Mutex::new(()),
            updates: RwLock::new(()),
            file_operations: RwLock::new(()),
            roots: State::new(Vec::new()),
            staging: Arc::new(Semaphore::new(4)),
            semantic_preparation: Mutex::new(()),
            state_changed: tokio::sync::Notify::new(),
            discovery: Mutex::new(None),
            discovery_epoch: AtomicU64::new(0),
            discovery_closed: AtomicBool::new(false),
            discovery_failure: tokio::sync::watch::channel(false).0,
        }
    }
}
impl WorkspaceService {
    pub(super) async fn document_operation(&self) -> WorkspaceOperationGuard<'_> {
        match self.file_operations.try_read() {
            Ok(guard) => WorkspaceOperationGuard::Concurrent { _guard: guard },
            // A queued rename changes URI identity. Queue subsequent document
            // notifications exclusively so close/reopen order is preserved.
            Err(_) => WorkspaceOperationGuard::Ordered {
                _guard: self.file_operations.write().await,
            },
        }
    }
    pub(super) fn read(&self) -> WorkspaceRead<'_> {
        WorkspaceRead(self.state.read())
    }
    pub(super) fn generation(&self) -> u64 {
        self.state.read().generation
    }
    pub(super) fn begin_revision(
        &self,
        uri: &Url,
        path: Option<PathBuf>,
        revision: Revision,
        create: bool,
    ) -> Option<(FileId, RevisionTicket, Option<PathBuf>)> {
        let mut state = self.state.write();
        let id = if create {
            state.database.ensure_file_id(uri.clone(), path)
        } else {
            state.database.file_id_by_uri(uri)?
        };
        let document = state.database.open_document(uri);
        let path = state.database.document_path(uri);
        let sync = state
            .revisions
            .entry(id)
            .or_insert_with(|| Arc::new(DocumentRevisionSync::new(document)))
            .clone();
        let ticket = revision_result(sync.reserve(revision))?;
        self.state_changed.notify_waiters();
        Some((id, ticket, path))
    }
    pub(super) fn begin_refresh(&self, uri: &Url, revision: Revision) -> Option<RevisionTicket> {
        let state = self.state.write();
        let id = state.database.file_id_by_uri(uri)?;
        revision_result(state.revisions.get(&id)?.reserve_refresh(revision))
    }
    pub(super) fn begin_close(&self, uri: &Url) -> Option<ClosedRevision> {
        let state = self.state.write();
        let file = state.database.file_id_by_uri(uri)?;
        let sync = state.revisions.get(&file)?;
        revision_result(sync.close());
        let generation = revision_result(sync.status()).generation();
        Some(ClosedRevision { file, generation })
    }

    pub(super) fn is_closed(&self, closed: ClosedRevision) -> bool {
        self.state.read().is_closed(closed)
    }

    pub(super) fn commit_close(&self, closed: ClosedRevision, uri: &Url) -> Option<FileId> {
        let mut state = self.state.write();
        if !state.is_closed(closed) || state.database.file_id_by_uri(uri) != Some(closed.file) {
            return None;
        }
        let result = state.apply(|database| database.close_document_prepared(uri));
        if result.is_some() {
            self.state_changed.notify_waiters();
        }
        result
    }
    /// Validation and all database/index changes share the same exclusive
    /// state guard as reservation and close. Snapshot construction happens first.
    pub(super) fn commit<T>(
        &self,
        ticket: Option<&RevisionTicket>,
        generation: Option<u64>,
        operation: impl FnOnce(&mut WorkspaceDb) -> Option<T>,
    ) -> Option<T> {
        let mut state = self.state.write();
        if generation.is_some_and(|generation| generation != state.generation)
            || ticket.is_some_and(|ticket| !revision_result(ticket.is_current()))
        {
            return None;
        }
        state.apply(operation)
    }
    pub(super) fn commit_file_rename<T>(
        &self,
        generation: u64,
        operation: impl FnOnce(&mut WorkspaceDb) -> Option<(Vec<(FileId, FileId)>, T)>,
    ) -> Option<T> {
        let mut state = self.state.write();
        if generation != state.generation
            || state
                .revisions
                .values()
                .any(|sync| revision_result(sync.status()).is_pending())
        {
            return None;
        }
        let (identities, result) = state.apply(operation)?;
        for (retired, canonical) in identities {
            if let Some(sync) = state.revisions.remove(&retired) {
                state.revisions.entry(canonical).or_insert(sync);
            }
        }
        Some(result)
    }
    pub(super) fn finish_revision(&self, ticket: &RevisionTicket) -> bool {
        let _state = self.state.write();
        revision_result(ticket.mark_committed())
    }

    pub(super) async fn invalidate_editor_import_paths(&self, events: &[(Url, PathBuf)]) {
        let _update = self.updates.write().await;
        let mut state = self.state.write();
        for (_, path) in events {
            state.database.invalidate_editor_import_path(path);
        }
    }

    pub(super) fn publish_editor_import_paths(
        &self,
        file: FileId,
        expected: &Arc<DocumentSnapshot>,
        paths: HashMap<PathBuf, Option<PathBuf>>,
    ) -> bool {
        let mut state = self.state.write();
        if state.revisions.get(&file).is_some_and(|sync| {
            let status = revision_result(sync.status());
            status.is_pending() || (status.desired().is_none() && state.database.is_file_open(file))
        }) {
            return false;
        }
        state
            .database
            .set_editor_import_paths(file, expected, paths)
    }

    /// Publishing a derived index must not invalidate source-generation captures
    /// held by refresh and file-operation staging.
    pub(super) fn publish_semantic_view(
        &self,
        view: Arc<SemanticView>,
        base: Option<&SemanticBase>,
    ) -> bool {
        let mut state = self.state.write();
        if state.database.semantic_view(view.root()).is_some()
            || view.documents().iter().any(|document| {
                let Some(file) = state.database.file_id_by_uri(&document.uri) else {
                    return false;
                };
                state.revisions.get(&file).is_some_and(|sync| {
                    let status = revision_result(sync.status());
                    status.is_pending()
                        || (status.desired().is_none() && state.database.is_file_open(file))
                })
            })
        {
            return false;
        }
        state.database.publish_semantic_view(view, base)
    }
}

#[derive(Default)]
pub(super) struct RegistrationState {
    pub(super) watch: bool,
    pub(super) type_hierarchy: bool,
    pub(super) versioned_edits: bool,
}

#[cfg(test)]
mod tests {
    use super::WorkspaceService;
    use crate::{analysis::Revision, workspace::Document};
    use std::{collections::HashMap, io};
    use url::Url;

    #[test]
    fn closing_reachable_overlay_invalidates_old_disk_and_imports() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("main.bend");
        let uri = Url::from_file_path(&path).map_err(|()| io::Error::other("file URI"))?;
        let service = WorkspaceService::default();
        let (_, ticket, _) = service
            .begin_revision(&uri, Some(path.clone()), Revision(1), true)
            .ok_or_else(|| io::Error::other("reserve open"))?;
        let old_source = "import ./old.bend as Old\ndef main = Old.value\n";
        let new_source = "import ./new.bend as New\ndef main = New.value\n";
        service
            .commit(Some(&ticket), None, |database| {
                Some(database.set_open_document(
                    Document::new(uri.clone(), "bend".into(), Revision(1), old_source.into()),
                    Some(path.clone()),
                ))
            })
            .ok_or_else(|| io::Error::other("commit open"))?;
        assert!(service.finish_revision(&ticket));
        drop(ticket);
        service
            .state
            .write()
            .database
            .sync_disk_path(&path, Some(old_source.into()));
        let consumer_path = temp.path().join("consumer.bend");
        let consumer_uri =
            Url::from_file_path(&consumer_path).map_err(|()| io::Error::other("consumer URI"))?;
        service.state.write().database.set_open_document(
            Document::new(
                consumer_uri,
                "bend".into(),
                Revision(1),
                "import ./main.bend as Main\ndef consumer = Main.main\n".into(),
            ),
            Some(consumer_path),
        );
        let closed = service
            .begin_close(&uri)
            .ok_or_else(|| io::Error::other("close epoch"))?;
        assert!(service.commit_close(closed, &uri).is_some());
        {
            let database = service.read();
            assert!(!database.is_document_open(&uri));
            assert!(database.cached_document(&uri).is_none());
            let id = database
                .file_id_by_uri(&uri)
                .ok_or_else(|| io::Error::other("root"))?;
            let old = database
                .file_id_by_path(&temp.path().join("old.bend"))
                .ok_or_else(|| io::Error::other("old target"))?;
            assert!(!database.dependencies(id).contains(&old));
            assert!(database.missing_disk_paths(&[id]).contains(&path));
        }
        service
            .state
            .write()
            .database
            .sync_disk_path(&path, Some(new_source.into()));
        let database = service.read();
        assert!(!database.is_document_open(&uri));
        let restored = database
            .cached_document(&uri)
            .ok_or_else(|| io::Error::other("restored disk"))?;
        assert_eq!(restored.text, new_source);
        let id = database
            .file_id_by_uri(&uri)
            .ok_or_else(|| io::Error::other("root"))?;
        let dependencies = database.dependencies(id);
        let old = database
            .file_id_by_path(&temp.path().join("old.bend"))
            .ok_or_else(|| io::Error::other("old target"))?;
        let new = database
            .file_id_by_path(&temp.path().join("new.bend"))
            .ok_or_else(|| io::Error::other("new target"))?;
        assert!(!dependencies.contains(&old));
        assert!(dependencies.contains(&new));
        Ok(())
    }

    #[test]
    fn queued_close_cannot_mutate_a_reopened_epoch() -> io::Result<()> {
        let service = WorkspaceService::default();
        let uri = Url::parse("file:///reopened.bend").map_err(io::Error::other)?;
        let (_, first, _) = service
            .begin_revision(&uri, None, Revision(8), true)
            .ok_or_else(|| io::Error::other("first epoch"))?;
        drop(first);
        let closed = service
            .begin_close(&uri)
            .ok_or_else(|| io::Error::other("close"))?;
        let (_, reopened, _) = service
            .begin_revision(&uri, None, Revision(1), true)
            .ok_or_else(|| io::Error::other("reopen epoch"))?;
        service
            .commit(Some(&reopened), None, |database| {
                Some(database.set_open_document(
                    Document::new(
                        uri.clone(),
                        "bend".into(),
                        Revision(1),
                        "def reopened = 1\n".into(),
                    ),
                    None,
                ))
            })
            .ok_or_else(|| io::Error::other("commit reopen"))?;
        assert!(service.commit_close(closed, &uri).is_none());
        assert_eq!(
            service
                .read()
                .open_document(&uri)
                .ok_or_else(|| io::Error::other("reopened document"))?
                .text,
            "def reopened = 1\n"
        );
        Ok(())
    }

    #[test]
    fn cold_semantic_publication_rejects_closed_and_reopened_inputs() -> io::Result<()> {
        let service = WorkspaceService::default();
        let uri = Url::parse("untitled:semantic-epoch").map_err(io::Error::other)?;
        let (root, first, _) = service
            .begin_revision(&uri, None, Revision(7), true)
            .ok_or_else(|| io::Error::other("first epoch"))?;
        service
            .commit(Some(&first), None, |database| {
                Some(database.set_open_document_prepared(
                    Document::new(
                        uri.clone(),
                        "bend".into(),
                        Revision(7),
                        "def old:\n  7\n".into(),
                    ),
                    None,
                    None,
                    Vec::new(),
                ))
            })
            .ok_or_else(|| io::Error::other("first commit"))?;
        assert!(service.finish_revision(&first));
        let input = service
            .read()
            .capture_semantic_view(root, None)
            .ok_or_else(|| io::Error::other("captured semantic input"))?;
        let stale = std::sync::Arc::new(input.build());
        let stale_snapshot = stale
            .document(root)
            .ok_or_else(|| io::Error::other("captured editor input"))?
            .snapshot
            .clone();
        let closed = service
            .begin_close(&uri)
            .ok_or_else(|| io::Error::other("close epoch"))?;
        assert!(service.read().is_file_open(root));
        assert!(!service.publish_semantic_view(stale.clone(), None));
        assert!(!service.publish_editor_import_paths(root, &stale_snapshot, HashMap::new()));
        assert!(service.commit_close(closed, &uri).is_some());
        assert!(!service.read().is_file_open(root));
        assert!(!service.publish_semantic_view(stale.clone(), None));
        assert!(!service.publish_editor_import_paths(root, &stale_snapshot, HashMap::new()));
        drop(first);
        let (_, reopened, _) = service
            .begin_revision(&uri, None, Revision(1), true)
            .ok_or_else(|| io::Error::other("reopened epoch"))?;
        service
            .commit(Some(&reopened), None, |database| {
                Some(database.set_open_document_prepared(
                    Document::new(
                        uri.clone(),
                        "bend".into(),
                        Revision(1),
                        "def fresh:\n  1\n".into(),
                    ),
                    None,
                    None,
                    Vec::new(),
                ))
            })
            .ok_or_else(|| io::Error::other("reopened commit"))?;
        assert!(service.finish_revision(&reopened));
        assert!(!service.publish_semantic_view(stale, None));
        assert!(!service.publish_editor_import_paths(root, &stale_snapshot, HashMap::new()));
        let current = service
            .read()
            .capture_semantic_view(root, None)
            .ok_or_else(|| io::Error::other("current semantic input"))?;
        let current = std::sync::Arc::new(current.build());
        let generation = service.generation();
        assert!(service.publish_semantic_view(current, None));
        assert_eq!(
            service.generation(),
            generation,
            "derived publication is not a source mutation"
        );
        assert!(service.read().semantic_views_ready_for(Some(root)));
        Ok(())
    }

    #[tokio::test]
    async fn enabled_close_waiter_observes_commit_and_reopen_notifications() -> io::Result<()> {
        let service = WorkspaceService::default();
        let uri = Url::parse("untitled:close-signal").map_err(io::Error::other)?;
        let (_, first, _) = service
            .begin_revision(&uri, None, Revision(1), true)
            .ok_or_else(|| io::Error::other("reserve first epoch"))?;
        service
            .commit(Some(&first), None, |database| {
                Some(database.set_open_document_prepared(
                    Document::new(
                        uri.clone(),
                        "bend".into(),
                        Revision(1),
                        "def main:\n  1\n".into(),
                    ),
                    None,
                    None,
                    Vec::new(),
                ))
            })
            .ok_or_else(|| io::Error::other("commit first epoch"))?;
        assert!(service.finish_revision(&first));
        let closed = service
            .begin_close(&uri)
            .ok_or_else(|| io::Error::other("close epoch"))?;
        let committed = service.state_changed.notified();
        tokio::pin!(committed);
        committed.as_mut().enable();
        assert!(service.commit_close(closed, &uri).is_some());
        tokio::time::timeout(std::time::Duration::from_secs(1), committed)
            .await
            .map_err(io::Error::other)?;
        let reopened = service.state_changed.notified();
        tokio::pin!(reopened);
        reopened.as_mut().enable();
        let _next = service
            .begin_revision(&uri, None, Revision(1), true)
            .ok_or_else(|| io::Error::other("reserve reopened epoch"))?;
        tokio::time::timeout(std::time::Duration::from_secs(1), reopened)
            .await
            .map_err(io::Error::other)?;
        Ok(())
    }
}
