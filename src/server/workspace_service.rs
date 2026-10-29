use super::{
    revision::{DocumentRevisionSync, RevisionStatus, RevisionTicket},
    state::{State, revision_result},
};
use crate::{
    analysis::Revision,
    workspace::{FileId, WorkspaceDb, views::ViewId},
};
use std::{
    borrow::Cow,
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
    // One revision stream per open editor spelling. LSP versions are per URI,
    // so two spellings of one file must not share a monotonic stream.
    pub(super) revisions: HashMap<ViewId, Arc<DocumentRevisionSync>>,
    generation: u64,
}

#[derive(Clone, Copy)]
pub(super) struct ClosedRevision {
    file: FileId,
    view: ViewId,
    generation: u64,
}

pub(super) struct PreparedRevision {
    pub(super) file: FileId,
    pub(super) ticket: RevisionTicket,
    pub(super) path: Option<PathBuf>,
    pub(super) uri: Option<Arc<Url>>,
}

impl WorkspaceState {
    pub(super) fn is_closed(&self, closed: ClosedRevision) -> bool {
        self.revisions.get(&closed.view).is_some_and(|sync| {
            let status = revision_result(sync.status());
            status.generation() == closed.generation && status.desired().is_none()
        })
    }

    pub(super) fn revision_sync(&self, uri: &Url) -> Option<&Arc<DocumentRevisionSync>> {
        let view = self.database.view_id(uri)?;
        self.revisions.get(&view)
    }

    pub(super) fn revision_status(&self, uri: &Url) -> Option<RevisionStatus> {
        Some(revision_result(self.revision_sync(uri)?.status()))
    }

    /// Raw sampled generation for a spelling, including a pending view. Fences
    /// sample this before capture and use `revision_current` afterwards.
    pub(super) fn revision_generation(&self, uri: &Url) -> Option<u64> {
        Some(self.revision_status(uri)?.generation())
    }

    pub(super) fn revision_current(&self, uri: &Url, generation: u64) -> bool {
        self.database.is_document_open(uri)
            && self
                .revision_status(uri)
                .is_some_and(|status| !status.is_pending() && status.generation() == generation)
    }

    /// Status of the spelling that represents this file, falling back to its
    /// latest closed spelling so staged disk-load and closed-file fences keep
    /// their revision status.
    pub(super) fn file_revision_status(&self, id: FileId) -> Option<RevisionStatus> {
        let view = self
            .database
            .representative_view_id(id)
            .or_else(|| self.database.latest_view_id(id))?;
        let sync = self.revisions.get(&view)?;
        Some(revision_result(sync.status()))
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
    ) -> Option<PreparedRevision> {
        let mut state = self.state.write();
        let id = if create {
            // Resolve the binding once so interning reuses the installed URI
            // allocation instead of wrapping a cloned Url in a fresh Arc.
            let shared = state.database.shared_uri(Cow::Borrowed(uri));
            state.database.ensure_file_id(shared, path)
        } else {
            state.database.file_id_by_uri(uri)?
        };
        let path = state.database.document_path(uri);
        let (view, shared_uri) = if create {
            let (view, uri) = state.database.ensure_view_shared(uri, id);
            (view, Some(uri))
        } else {
            (state.database.ensure_view(uri, id), None)
        };
        let state = &mut *state;
        let sync = state
            .revisions
            .entry(view)
            .or_insert_with(|| {
                Arc::new(DocumentRevisionSync::new(state.database.open_document(uri)))
            })
            .clone();
        let ticket = revision_result(sync.reserve(revision))?;
        Some(PreparedRevision {
            file: id,
            ticket,
            path,
            uri: shared_uri,
        })
    }
    pub(super) fn begin_refresh(&self, uri: &Url, revision: Revision) -> Option<RevisionTicket> {
        let state = self.state.write();
        let view = state.database.view_id(uri)?;
        revision_result(state.revisions.get(&view)?.reserve_refresh(revision))
    }
    pub(super) fn begin_close(&self, uri: &Url) -> Option<ClosedRevision> {
        let state = self.state.write();
        let file = state.database.file_id_by_uri(uri)?;
        let view = state.database.view_id(uri)?;
        let sync = state.revisions.get(&view)?;
        revision_result(sync.close());
        let generation = revision_result(sync.status()).generation();
        Some(ClosedRevision {
            file,
            view,
            generation,
        })
    }

    pub(super) fn is_closed(&self, closed: ClosedRevision) -> bool {
        self.state.read().is_closed(closed)
    }

    pub(super) fn commit_close(&self, closed: ClosedRevision, uri: &Url) -> Option<FileId> {
        let mut state = self.state.write();
        if !state.is_closed(closed)
            || state.database.file_id_by_uri(uri) != Some(closed.file)
            || state.database.view_id(uri) != Some(closed.view)
        {
            return None;
        }
        state.apply(|database| database.close_document_prepared(uri))
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
        // View epochs are re-keyed by `WorkspaceDb::rename_paths` (a moved
        // buffer keeps its revision stream), so merged file identities need no
        // further revision bookkeeping here.
        let (_identities, result) = state.apply(operation)?;
        Some(result)
    }
    pub(super) fn finish_revision(&self, ticket: &RevisionTicket) -> bool {
        let _state = self.state.write();
        revision_result(ticket.mark_committed())
    }
}

#[derive(Default)]
pub(super) struct RegistrationState {
    pub(super) watch: bool,
    pub(super) type_hierarchy: bool,
    pub(super) lint: LintClientCapabilities,
}

#[derive(Default)]
pub(super) struct LintClientCapabilities {
    pub(super) disabled_code_actions: bool,
    pub(super) work_done_progress: bool,
}

#[cfg(test)]
mod tests {
    use super::WorkspaceService;
    use crate::{analysis::Revision, workspace::Document};
    use std::io;
    use url::Url;

    #[test]
    fn closing_reachable_overlay_invalidates_old_disk_and_imports() -> io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("main.bend");
        let uri = Url::from_file_path(&path).map_err(|()| io::Error::other("file URI"))?;
        let service = WorkspaceService::default();
        let ticket = service
            .begin_revision(&uri, Some(path.clone()), Revision(1), true)
            .ok_or_else(|| io::Error::other("reserve open"))?
            .ticket;
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
        let first = service
            .begin_revision(&uri, None, Revision(8), true)
            .ok_or_else(|| io::Error::other("first epoch"))?
            .ticket;
        drop(first);
        let closed = service
            .begin_close(&uri)
            .ok_or_else(|| io::Error::other("close"))?;
        let reopened = service
            .begin_revision(&uri, None, Revision(1), true)
            .ok_or_else(|| io::Error::other("reopen epoch"))?
            .ticket;
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
}
