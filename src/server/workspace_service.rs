use super::{
    revision::{DocumentRevisionSync, RevisionTicket},
    state::{State, revision_result},
};
use crate::{
    analysis::{Revision, TextRange},
    workspace::{FileId, PreparedSemanticSnapshot, WorkspaceDb},
};
use std::{
    collections::HashMap,
    ops::Deref,
    path::PathBuf,
    sync::{Arc, RwLockReadGuard},
};
use tokio::sync::{Mutex, RwLock, Semaphore};
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

pub(super) enum CloseCommit {
    Committed(FileId),
    RetryDisk,
    Superseded,
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

pub(super) struct WorkspaceService {
    pub(super) state: State<WorkspaceState>,
    pub(super) update_serial: Mutex<()>,
    pub(super) updates: RwLock<()>,
    pub(super) roots: State<Vec<PathBuf>>,
    pub(super) staging: Arc<Semaphore>,
    pub(super) discovery: Arc<super::discovery::DiscoveryService>,
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
            roots: State::new(Vec::new()),
            staging: Arc::new(Semaphore::new(4)),
            discovery: Arc::new(super::discovery::DiscoveryService::default()),
        }
    }
}
impl WorkspaceService {
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

    pub(super) fn commit_close(
        &self,
        closed: ClosedRevision,
        uri: &Url,
        imports: Vec<(TextRange, PathBuf)>,
        semantics: Option<PreparedSemanticSnapshot>,
    ) -> CloseCommit {
        let mut state = self.state.write();
        if !state.is_closed(closed) || state.database.file_id_by_uri(uri) != Some(closed.file) {
            return CloseCommit::Superseded;
        }
        // The database compares the prepared disk snapshot identity before
        // mutation. Only changes to this file invalidate restoration; unrelated
        // discovery commits must not abandon an otherwise-current close.
        match state.apply(|database| database.close_document_prepared(uri, imports, semantics)) {
            Some(file) => CloseCommit::Committed(file),
            None => CloseCommit::RetryDisk,
        }
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
