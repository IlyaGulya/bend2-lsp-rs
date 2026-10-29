use super::{
    compiler::CompilerReapers,
    compiler_service::BaseModule,
    lsp::Backend,
    revision::{RevisionStatus, RevisionTicket, wait_for_captured_revision},
    state::revision_result,
};
use crate::{
    analysis::{self, DocumentSnapshot, LineIndex, Revision},
    workspace::{Document, FileId, PreparedImportTarget, SourceGraph},
};
use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{RwLockReadGuard, Semaphore, watch};
use tower_lsp::{Client, lsp_types::DidChangeTextDocumentParams};
use tracing::Instrument;
use url::Url;

struct WatchedFile {
    id: FileId,
    disk_generation: u64,
    uri: Url,
    path: PathBuf,
}

struct PreparedDiskUpdate {
    file: WatchedFile,
    snapshot: Option<Arc<DocumentSnapshot>>,
    physical_path: PathBuf,
    imports: Vec<PreparedImportTarget>,
}

struct WatchedImporter {
    id: FileId,
    path: PathBuf,
    snapshot: Arc<DocumentSnapshot>,
}

struct PreparedImporter {
    id: FileId,
    snapshot: Arc<DocumentSnapshot>,
    physical_path: PathBuf,
    imports: Vec<PreparedImportTarget>,
}

pub(super) async fn run_staging<T, F>(semaphore: Arc<Semaphore>, operation: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let span = tracing::Span::current();
    let permit = semaphore.acquire_owned().await.ok()?;
    Some(super::state::blocking_result(
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let _permit = permit;
                operation()
            })
        })
        .await,
    ))
}

pub(super) fn trace_document_snapshot(revision: Revision, text: String) -> DocumentSnapshot {
    let source_bytes = text.len();
    let span = tracing::info_span!(
        "snapshot.build",
        revision = revision.0,
        source_bytes,
        token_count = tracing::field::Empty,
        import_count = tracing::field::Empty,
        symbol_count = tracing::field::Empty,
        call_count = tracing::field::Empty,
    );
    let snapshot = span.in_scope(|| DocumentSnapshot::new(revision, text));
    record_snapshot_counts(&span, &snapshot);
    snapshot
}

pub(super) fn trace_document_snapshot_with_line_index(
    revision: Revision,
    text: String,
    line_index: LineIndex,
) -> DocumentSnapshot {
    let source_bytes = text.len();
    let span = tracing::info_span!(
        "snapshot.build",
        revision = revision.0,
        source_bytes,
        token_count = tracing::field::Empty,
        import_count = tracing::field::Empty,
        symbol_count = tracing::field::Empty,
        call_count = tracing::field::Empty,
    );
    let snapshot = span.in_scope(|| DocumentSnapshot::with_line_index(revision, text, line_index));
    record_snapshot_counts(&span, &snapshot);
    snapshot
}

pub(super) fn record_snapshot_counts(span: &tracing::Span, snapshot: &DocumentSnapshot) {
    if span.is_disabled() {
        return;
    }
    span.record("token_count", snapshot.syntax.tokens().len());
    span.record("import_count", snapshot.syntax.imports().len());
    span.record("symbol_count", snapshot.syntax.symbols().len());
    span.record("call_count", snapshot.syntax.calls().len());
}

impl Backend {
    pub(super) fn new(
        client: Client,
        workspace: Arc<super::workspace_service::WorkspaceService>,
        diagnostics: Arc<super::diagnostics::DiagnosticsService>,
        compiler_reapers: Arc<CompilerReapers>,
    ) -> Self {
        Self {
            client,
            workspace,
            compiler: Arc::new(super::compiler_service::CompilerService::new(
                compiler_reapers,
            )),
            diagnostics,
            registration: Arc::new(super::state::State::new(
                super::workspace_service::RegistrationState::default(),
            )),
        }
    }
    pub(super) fn begin_revision(
        &self,
        uri: &Url,
        path: Option<PathBuf>,
        revision: Revision,
        create: bool,
    ) -> Option<(FileId, RevisionTicket, Option<PathBuf>)> {
        self.workspace.begin_revision(uri, path, revision, create)
    }
    pub(super) fn begin_document_refresh(
        &self,
        uri: &Url,
        revision: Revision,
    ) -> Option<RevisionTicket> {
        self.workspace.begin_refresh(uri, revision)
    }

    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "open", revision = version, source_bytes = text.len(), file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    pub(super) async fn open_workspace_text(
        &self,
        uri: Url,
        language_id: String,
        version: i32,
        text: String,
        path: Option<PathBuf>,
    ) -> Option<()> {
        let (id, ticket, path) = self.begin_revision(&uri, path, Revision(version), true)?;
        tracing::Span::current().record("file_id", tracing::field::debug(&id));
        if !revision_result(ticket.wait_for_turn().await) {
            return None;
        }
        let build_path = path.clone();
        let opened = run_staging(self.workspace.staging.clone(), move || {
            let snapshot = Arc::new(trace_document_snapshot(Revision(version), text));
            let document = Document::with_snapshot(uri, language_id, snapshot);
            let physical_path = build_path
                .as_deref()
                .map(crate::workspace::canonical_source_path);
            let imports = build_path
                .as_deref()
                .zip(physical_path.as_deref())
                .map_or_else(Vec::new, |(path, physical_path)| {
                    crate::workspace::resolve_import_targets(
                        path,
                        physical_path,
                        &document.snapshot,
                    )
                });
            let library_path = crate::workspace::canonical_library_path();
            (document, imports, physical_path, library_path)
        })
        .await?;
        let (document, imports, physical_path, library_path) = opened;
        revision_result(ticket.store_staged_document(document.clone()));
        if !revision_result(ticket.is_current()) {
            return None;
        }
        let root = {
            let _workspace_update = self.workspace.updates.write().await;
            // The workspace service validates under the same lock as mutation.
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "open",
                file_count = 1,
                file_id = tracing::field::Empty,
                outcome = tracing::field::Empty,
            );
            let root = commit_span.in_scope(|| {
                self.workspace.commit(Some(&ticket), None, |database| {
                    database.set_semantic_library_path(library_path);
                    Some(database.set_open_document_prepared(
                        document,
                        path,
                        physical_path,
                        imports,
                    ))
                })
            })?;
            commit_span.record("file_id", tracing::field::debug(&root));
            commit_span.record("outcome", "committed");
            root
        };
        self.workspace
            .load_reachable_async(std::slice::from_ref(&root), None)
            .await;
        if !self.workspace.finish_revision(&ticket) {
            return None;
        }
        self.prepare_semantic_views().await;
        tracing::Span::current().record("outcome", "committed");
        drop(ticket);
        Some(())
    }

    #[tracing::instrument(
    name = "document.wait_revision",
    skip_all,
    fields(
        file_id = tracing::field::Empty,
        desired_revision = tracing::field::Empty,
        committed_revision = tracing::field::Empty,
        outcome = tracing::field::Empty,
    )
)]
    pub(super) async fn wait_for_document_revision(&self, uri: &Url) {
        let span = tracing::Span::current();
        let sync = {
            let state = self.workspace.state.read();
            let revisions = &state.revisions;
            let database = &state.database;
            let Some(id) = database.file_id_by_uri(uri) else {
                span.record("outcome", "untracked");
                return;
            };
            span.record("file_id", tracing::field::debug(&id));
            revisions.get(&id).cloned()
        };
        let Some(sync) = sync else {
            span.record("outcome", "untracked");
            return;
        };
        let mut status = sync.subscribe();
        loop {
            let state = revision_result((*status.borrow_and_update()).checked());
            span.record(
                "desired_revision",
                state.desired().map_or(-1, |revision| revision.0),
            );
            span.record(
                "committed_revision",
                state.committed().map_or(-1, |revision| revision.0),
            );
            if state.desired().is_none() {
                span.record("outcome", "closed");
                return;
            }
            if state.committed() == state.desired() {
                span.record("outcome", "ready");
                return;
            }
            if state.is_failed() {
                span.record("outcome", "failed");
                return;
            }
            if status.changed().await.is_err() {
                span.record("outcome", "closed");
                return;
            }
        }
    }

    pub(super) async fn document_read(&self, uri: &Url) -> RwLockReadGuard<'_, ()> {
        self.wait_for_document_revision(uri).await;
        self.ready_read(Some(uri)).await
    }

    async fn workspace_read(&self) -> RwLockReadGuard<'_, ()> {
        // Cold snapshot construction never holds this committed-view guard.
        self.workspace
            .updates
            .read()
            .instrument(tracing::info_span!("workspace.sync_wait"))
            .await
    }
    pub(super) fn pending_revisions(
        &self,
        uri: Option<&Url>,
    ) -> (Vec<(watch::Receiver<RevisionStatus>, u64)>, bool) {
        let state = self.workspace.state.read();
        let revisions = &state.revisions;
        let database = &state.database;
        // The normal path neither walks imports nor allocates waiters. Closing
        // epochs use the existing status plus DB overlay, not a second lifecycle.
        if !revisions.iter().any(|(file, sync)| {
            let status = revision_result(sync.status());
            status.is_pending() || (status.desired().is_none() && database.is_file_open(*file))
        }) {
            return (Vec::new(), false);
        }
        let dependencies = if let Some(uri) = uri {
            let Some(root) = database.file_id_by_uri(uri) else {
                return (Vec::new(), false);
            };
            Some(database.dependencies(root))
        } else {
            None
        };
        let mut closing = false;
        let pending = revisions
            .iter()
            .filter_map(|(file, sync)| {
                if dependencies
                    .as_ref()
                    .is_some_and(|dependencies| !dependencies.contains(file))
                {
                    return None;
                }
                let status = revision_result(sync.status());
                closing |= status.desired().is_none() && database.is_file_open(*file);
                status
                    .is_pending()
                    .then(|| (sync.subscribe(), status.generation()))
            })
            .collect();
        (pending, closing)
    }

    pub(super) async fn ready_read(&self, uri: Option<&Url>) -> RwLockReadGuard<'_, ()> {
        let _operation = self.workspace.file_operations.read().await;
        self.ready_read_inner(uri).await
    }

    pub(super) async fn file_rename_ready_read(&self) -> RwLockReadGuard<'_, ()> {
        // The rename entry point already holds the exclusive operation guard.
        self.ready_read_inner(None).await
    }

    async fn ready_read_inner(&self, uri: Option<&Url>) -> RwLockReadGuard<'_, ()> {
        if uri.is_none_or(|uri| !self.workspace.read().is_document_open(uri)) {
            self.workspace.wait_for_discovery().await;
        }
        loop {
            let changed = self.workspace.state_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let read = self.workspace_read().await;
            let (pending, closing) = self.pending_revisions(uri);
            if pending.is_empty() {
                if closing {
                    drop(read);
                    changed.await;
                    continue;
                }
                let (ready, root) = {
                    let database = self.workspace.read();
                    let root = uri.and_then(|uri| database.file_id_by_uri(uri));
                    let ready = match uri {
                        Some(_) => {
                            root.is_none_or(|root| database.semantic_views_ready_for(Some(root)))
                        }
                        None => true,
                    };
                    (ready, root)
                };
                if ready {
                    return read;
                }
                drop(read);
                // Discovery and superseding revisions can dirty a view after an
                // update owner's preparation. Capture/build off-lock and recheck.
                if let Some(root) = root {
                    self.prepare_semantic_views_for_read(std::slice::from_ref(&root))
                        .await;
                }
                continue;
            }
            drop(read);
            let span = tracing::info_span!(
                "workspace.readiness_wait",
                captured_pending_count = pending.len()
            );
            async move {
                for (status, generation) in pending {
                    revision_result(wait_for_captured_revision(status, generation).await);
                }
            }
            .instrument(span)
            .await;
            // A waited-for generation can be superseded, or its imports can
            // change. Recheck relevant revisions under the committed-view guard.
        }
    }

    pub(super) async fn workspace_ready_read(&self) -> RwLockReadGuard<'_, ()> {
        self.ready_read(None).await
    }

    /// Query source, including needed closed files and compiler-leased snapshots.
    /// Overlay lifecycle decisions must use `open_document` / `is_document_open`.
    pub(super) fn document(&self, uri: &Url) -> Option<Document> {
        let database = self.workspace.read();
        let id = database.file_id_by_uri(uri)?;
        if !database.is_needed(id) && !database.is_compiler_document(uri) {
            return None;
        }
        let document = database.cached_document(uri)?;
        if tracing::enabled!(
            target: "bend2_lsp::server::lsp",
            tracing::Level::INFO
        ) {
            let span = tracing::Span::current();
            span.record("file_id", tracing::field::debug(&id));
            span.record("revision", document.revision.0);
        }
        Some(document)
    }

    pub(super) async fn prepare_editor_import_paths(&self, uri: &Url) {
        let captured = {
            let database = self.workspace.read();
            if database.editor_import_paths_ready(uri) {
                return;
            }
            database.file_id_by_uri(uri).and_then(|file| {
                let document = database.cached_document(uri)?;
                let path = database.document_path(uri)?;
                Some((file, document.snapshot, path))
            })
        };
        let Some((file, snapshot, path)) = captured else {
            return;
        };
        let Some((snapshot, paths)) = run_staging(self.workspace.staging.clone(), move || {
            let paths = crate::workspace::resolve_editor_import_paths(&path, &snapshot);
            (snapshot, paths)
        })
        .await
        else {
            return;
        };
        let _update = self.workspace.updates.write().await;
        self.workspace
            .publish_editor_import_paths(file, &snapshot, paths);
    }

    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "open_documents", result_count = tracing::field::Empty))]
    pub(super) fn workspace_documents(&self) -> Vec<Document> {
        let documents = self.workspace.read().open_documents();
        tracing::Span::current().record("result_count", documents.len());
        documents
    }
    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "refresh", revision = document.revision.0, source_bytes = document.text.len(), file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    pub(super) async fn open_workspace_document(&self, document: Document, path: Option<PathBuf>) {
        let Some(ticket) = self.begin_document_refresh(&document.uri, document.revision) else {
            return;
        };
        if !revision_result(ticket.wait_for_turn().await) {
            return;
        }
        let _serial = self.workspace.update_serial.lock().await;
        let generation = self.workspace.generation();
        let Some((document, path, physical_path, library_path, imports)) =
            run_staging(self.workspace.staging.clone(), move || {
                let physical_path = path.as_deref().map(crate::workspace::canonical_source_path);
                let imports = path.as_deref().zip(physical_path.as_deref()).map_or_else(
                    Vec::new,
                    |(path, physical_path)| {
                        crate::workspace::resolve_import_targets(
                            path,
                            physical_path,
                            &document.snapshot,
                        )
                    },
                );
                let library_path = crate::workspace::canonical_library_path();
                (document, path, physical_path, library_path, imports)
            })
            .await
        else {
            return;
        };
        let root = {
            let _workspace_update = self.workspace.updates.write().await;
            // Generation and ticket validation are atomic with the commit.
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "refresh",
                file_count = 1,
                file_id = tracing::field::Empty,
                outcome = tracing::field::Empty,
            );
            let Some(root) = commit_span.in_scope(|| {
                self.workspace
                    .commit(Some(&ticket), Some(generation), |database| {
                        database.set_semantic_library_path(library_path);
                        Some(database.set_open_document_prepared(
                            document,
                            path,
                            physical_path,
                            imports,
                        ))
                    })
            }) else {
                return;
            };
            commit_span.record("file_id", tracing::field::debug(&root));
            commit_span.record("outcome", "committed");
            root
        };
        tracing::Span::current().record("file_id", tracing::field::debug(&root));
        self.workspace
            .load_reachable_async(std::slice::from_ref(&root), None)
            .await;
        if self.workspace.finish_revision(&ticket) {
            tracing::Span::current().record("outcome", "committed");
        } else {
            tracing::Span::current().record("outcome", "superseded");
        }
        self.prepare_semantic_views().await;
    }

    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "revision", revision = params.text_document.version, file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    pub(super) async fn change_workspace_document(
        &self,
        params: DidChangeTextDocumentParams,
    ) -> bool {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        let Some((id, ticket, path)) = self.begin_revision(&uri, None, Revision(version), false)
        else {
            return false;
        };
        if !revision_result(ticket.wait_for_turn().await) {
            return false;
        }
        let Some(document) = revision_result(ticket.staged_document())
            .or_else(|| self.workspace.read().open_document(&uri))
        else {
            return false;
        };
        let changes = params.content_changes;
        let Some((updated_document, physical_path, library_path, imports)) =
            run_staging(self.workspace.staging.clone(), move || {
                let (text, line_index) =
                    super::document_text::DocumentText::apply_changes(&document, changes)
                        .into_parts();
                let snapshot = Arc::new(trace_document_snapshot_with_line_index(
                    Revision(version),
                    text,
                    line_index,
                ));
                let updated_document = Document::with_snapshot(
                    document.uri.clone(),
                    document.language_id.clone(),
                    snapshot.clone(),
                );
                let physical_path = path.as_deref().map(crate::workspace::canonical_source_path);
                let imports = path.as_deref().zip(physical_path.as_deref()).map_or_else(
                    Vec::new,
                    |(path, physical_path)| {
                        crate::workspace::resolve_import_targets(path, physical_path, &snapshot)
                    },
                );
                let library_path = crate::workspace::canonical_library_path();
                (updated_document, physical_path, library_path, imports)
            })
            .await
        else {
            return false;
        };
        revision_result(ticket.store_staged_document(updated_document.clone()));
        if !revision_result(ticket.is_current()) {
            return false;
        }
        let imports_changed = {
            let _workspace_update = self.workspace.updates.write().await;
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "revision",
                file_id = tracing::field::debug(&id),
                outcome = tracing::field::Empty,
                imports_changed = tracing::field::Empty,
            );
            let result = commit_span.in_scope(|| {
                self.workspace.commit(Some(&ticket), None, |database| {
                    database.set_semantic_library_path(library_path);
                    let (_, imports_changed) = database.update_open_snapshot_prepared(
                        &uri,
                        updated_document.snapshot.clone(),
                        physical_path,
                        imports,
                    )?;
                    Some(imports_changed)
                })
            });
            let Some(imports_changed) = result else {
                return false;
            };
            commit_span.record("imports_changed", imports_changed);
            commit_span.record("outcome", "committed");
            imports_changed
        };
        if imports_changed {
            self.workspace
                .load_reachable_async(std::slice::from_ref(&id), None)
                .await;
        }
        if !self.workspace.finish_revision(&ticket) {
            return false;
        }
        self.prepare_semantic_views().await;
        tracing::Span::current().record("outcome", "committed");
        true
    }
    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "close", file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    pub(super) async fn close_workspace_document(
        &self,
        uri: Url,
        closed: super::workspace_service::ClosedRevision,
    ) -> bool {
        let id = {
            let _workspace_update = self.workspace.updates.write().await;
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "close",
                file_id = tracing::field::Empty,
                outcome = tracing::field::Empty,
            );
            let Some(id) = commit_span.in_scope(|| self.workspace.commit_close(closed, &uri))
            else {
                commit_span.record("outcome", "superseded");
                return false;
            };
            commit_span.record("file_id", tracing::field::debug(&id));
            commit_span.record("outcome", "committed");
            tracing::Span::current().record("file_id", tracing::field::debug(&id));
            id
        };
        self.workspace
            .load_reachable_async(std::slice::from_ref(&id), None)
            .await;
        self.prepare_semantic_views().await;
        tracing::Span::current().record("outcome", "committed");
        true
    }

    pub(super) fn document_path(&self, uri: &Url) -> Option<PathBuf> {
        uri.to_file_path()
            .ok()
            .or_else(|| self.virtual_document_path(uri))
    }

    pub(super) fn virtual_document_path(&self, uri: &Url) -> Option<PathBuf> {
        if uri.scheme() != "untitled" {
            return None;
        }
        let root = self.workspace.roots.read().first()?.clone();
        let mut hasher = DefaultHasher::new();
        uri.hash(&mut hasher);
        Some(root.join(format!(".bend2-lsp-virtual-{:016x}.bend", hasher.finish())))
    }

    pub(super) fn module_document(
        &self,
        source: &Document,
        alias: &str,
    ) -> Option<(Url, Arc<DocumentSnapshot>)> {
        let import = analysis::imports(source)
            .iter()
            .find(|import| import.valid_syntax && import.alias_text(&source.text) == Some(alias))?;
        let document = self
            .workspace
            .read()
            .import_target(&source.uri, import.path)?;
        Some((document.uri, document.snapshot))
    }

    pub(super) fn prelude_module(&self, source: &Document) -> Option<BaseModule> {
        if !analysis::imports(source).iter().any(|import| {
            import.valid_syntax
                && import.alias.is_none()
                && import.path_text(&source.text) == "Base"
        }) {
            return None;
        }
        self.compiler.base_module.read().clone()
    }

    pub(super) fn supported(doc: &Document) -> bool {
        doc.language_id == "bend" || doc.language_id == "bend2"
    }
    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "source_graph", file_id = tracing::field::Empty))]
    pub(super) async fn compiler_source_graph(&self, uri: Url) -> Option<SourceGraph> {
        let span = tracing::Span::current();
        let permit = self
            .diagnostics
            .analysis
            .clone()
            .acquire_owned()
            .await
            .ok()?;
        let workspace_db = self.workspace.clone();
        super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                span.in_scope(|| {
                    let _permit = permit;
                    let database = workspace_db.read();
                    let root = database.file_id_by_uri(&uri)?;
                    tracing::Span::current().record("file_id", tracing::field::debug(&root));
                    database.source_graph(root)
                })
            })
            .await,
        )
    }

    /// Dependents that can publish diagnostics: only open overlays own a
    /// diagnostics task. `run_diagnostics` rejects a closed document after its
    /// revision delay, so scheduling one would allocate and wake a late no-op.
    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "dependents", file_id = tracing::field::Empty, result_count = tracing::field::Empty))]
    pub(super) async fn dependent_documents(&self, changed: &Url) -> Vec<Document> {
        let span = tracing::Span::current();
        let Ok(permit) = self.diagnostics.analysis.clone().acquire_owned().await else {
            return Vec::new();
        };
        let workspace_db = self.workspace.clone();
        let uri = changed.clone();
        let path = self.document_path(changed);
        super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                span.in_scope(|| {
                    let _permit = permit;
                    let database = workspace_db.read();
                    let id = database.file_id_by_uri(&uri).or_else(|| {
                        path.as_deref()
                            .and_then(|path| database.file_id_by_path(path))
                    });
                    let documents = id.map_or_else(Vec::new, |id| {
                        tracing::Span::current().record("file_id", tracing::field::debug(&id));
                        database.open_dependents(id)
                    });
                    tracing::Span::current().record("result_count", documents.len());
                    documents
                })
            })
            .await,
        )
    }

    fn watched_snapshot_context(
        &self,
        events: Vec<(Url, PathBuf)>,
    ) -> (Vec<WatchedFile>, Vec<WatchedImporter>, HashMap<Url, i32>) {
        let database = self.workspace.read();
        let mut admitted = Vec::new();
        let mut seen = HashSet::new();
        let mut affected = HashMap::new();
        let mut importers = HashMap::new();
        for (uri, path) in events {
            for document in database.importer_documents_for_path(&path) {
                let Some(id) = database.file_id_by_uri(&document.uri) else {
                    continue;
                };
                let Some(path) = database.document_path(&document.uri) else {
                    continue;
                };
                importers.entry(id).or_insert(WatchedImporter {
                    id,
                    path,
                    snapshot: document.snapshot,
                });
            }
            let Some(id) = database
                .file_id_by_uri(&uri)
                .or_else(|| database.file_id_by_path(&path))
            else {
                continue;
            };
            if !database.is_needed(id) || !seen.insert(id) {
                continue;
            }
            let Some(disk_generation) = database.disk_generation(id) else {
                continue;
            };
            admitted.push(WatchedFile {
                id,
                disk_generation,
                uri,
                path,
            });
            for document in database.dependents(id) {
                affected.insert(document.uri.clone(), document.revision.0);
            }
        }
        (admitted, importers.into_values().collect(), affected)
    }

    async fn prepare_watched_updates(
        semaphore: Arc<Semaphore>,
        events: Vec<WatchedFile>,
        importers: Vec<WatchedImporter>,
    ) -> Option<(
        Vec<PreparedDiskUpdate>,
        Vec<PreparedImporter>,
        Option<PathBuf>,
    )> {
        run_staging(semaphore, move || {
            let updates = events
                .into_iter()
                .map(|file| {
                    let snapshot = std::fs::read_to_string(&file.path)
                        .ok()
                        .map(|text| Arc::new(trace_document_snapshot(Revision::UNVERSIONED, text)));
                    let physical_path = crate::workspace::canonical_source_path(&file.path);
                    let imports = snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
                        crate::workspace::resolve_import_targets(
                            &file.path,
                            &physical_path,
                            snapshot,
                        )
                    });
                    PreparedDiskUpdate {
                        file,
                        snapshot,
                        physical_path,
                        imports,
                    }
                })
                .collect();
            let importers = importers
                .into_iter()
                .map(|importer| {
                    let physical_path = crate::workspace::canonical_source_path(&importer.path);
                    let imports = crate::workspace::resolve_import_targets(
                        &importer.path,
                        &physical_path,
                        &importer.snapshot,
                    );
                    PreparedImporter {
                        id: importer.id,
                        physical_path,
                        imports,
                        snapshot: importer.snapshot,
                    }
                })
                .collect();
            (
                updates,
                importers,
                crate::workspace::canonical_library_path(),
            )
        })
        .await
    }

    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "watched_files", event_count = events.len(), outcome = tracing::field::Empty))]
    pub(super) async fn sync_watched_files(
        &self,
        events: Vec<(Url, PathBuf)>,
    ) -> HashMap<Url, i32> {
        if events.is_empty() {
            return HashMap::new();
        }
        let _serial = self.workspace.update_serial.lock().await;
        // Reserve watcher order before joining the initial scan. Completion may
        // bypass that scan, but must not bypass a preceding watcher update.
        self.workspace.wait_for_discovery_worker().await;
        self.workspace.invalidate_editor_import_paths(&events).await;
        self.workspace.admit_watched_sources(&events).await;
        let (events, importers, mut affected) = self.watched_snapshot_context(events);
        if events.is_empty() && importers.is_empty() {
            return HashMap::new();
        }
        let Some((prepared, importers, library_path)) =
            Self::prepare_watched_updates(self.workspace.staging.clone(), events, importers).await
        else {
            return HashMap::new();
        };
        let updates = {
            let _workspace_update = self.workspace.updates.write().await;
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "watched_files",
                file_count = prepared.len(),
                outcome = tracing::field::Empty,
            );
            let updates = commit_span.in_scope(|| {
                self.workspace.commit(None, None, |database| {
                    database.set_semantic_library_path(library_path);
                    let mut updates = Vec::with_capacity(prepared.len());
                    for update in prepared {
                        if !database.is_needed(update.file.id)
                            || database.disk_generation(update.file.id)
                                != Some(update.file.disk_generation)
                        {
                            continue;
                        }
                        let Some((id, imports_changed)) = database.sync_disk_snapshot_prepared(
                            &update.file.path,
                            &update.physical_path,
                            update.snapshot,
                            update.imports,
                        ) else {
                            continue;
                        };
                        if let Some(document) = database.open_document(&update.file.uri) {
                            affected.insert(document.uri.clone(), document.revision.0);
                        }
                        updates.push((id, imports_changed));
                    }
                    for importer in importers {
                        if let Some(changed) = database.refresh_imports_prepared(
                            importer.id,
                            &importer.snapshot,
                            &importer.physical_path,
                            importer.imports,
                        ) {
                            updates.push((importer.id, changed));
                        }
                    }
                    Some(updates)
                })
            });
            let Some(updates) = updates else {
                return HashMap::new();
            };
            commit_span.record("outcome", "committed");
            updates
        };
        for (id, imports_changed) in &updates {
            if *imports_changed {
                self.workspace
                    .load_reachable_async(std::slice::from_ref(id), None)
                    .await;
            }
        }
        self.prepare_semantic_views().await;
        let workspace_db = self.workspace.clone();
        let Some(affected) = run_staging(self.workspace.staging.clone(), move || {
            let database = workspace_db.read();
            for (id, _) in updates {
                for document in database.dependents(id) {
                    affected.insert(document.uri.clone(), document.revision.0);
                }
            }
            affected.retain(|uri, version| {
                let Some(document) = database.open_document(uri) else {
                    return false;
                };
                *version = document.revision.0;
                true
            });
            affected
        })
        .await
        else {
            return HashMap::new();
        };
        tracing::Span::current().record("outcome", "committed");
        affected
    }
}
