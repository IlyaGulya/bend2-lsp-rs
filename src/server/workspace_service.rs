use super::{
    revision::{DocumentRevisionSync, RevisionTicket},
    state::{State, revision_result},
};
use crate::{
    analysis::Revision,
    workspace::{FileId, WorkspaceDb},
};
use std::{
    collections::HashMap,
    ops::Deref,
    path::PathBuf,
    sync::{Arc, RwLockReadGuard},
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
}

#[derive(Default)]
pub(super) struct RegistrationState {
    pub(super) watch: bool,
    pub(super) type_hierarchy: bool,
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
}
