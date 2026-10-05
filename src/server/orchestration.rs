use super::{
    compiler::CompilerReapers,
    compiler_service::BaseModule,
    features::named_document_symbol,
    lsp::Backend,
    reference_locations,
    revision::{RevisionStatus, RevisionTicket, wait_for_captured_revision},
    state::revision_result,
};
use crate::{
    analysis::{self, DocumentSnapshot, LineIndex, Revision},
    workspace::{
        Document, FileId, PreparedSemanticSnapshot, SourceGraph, prepare_semantic_snapshot,
    },
};
use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{RwLockReadGuard, Semaphore, watch};
use tower_lsp::{
    Client,
    lsp_types::{DidChangeTextDocumentParams, Location},
};
use tracing::Instrument;
use url::Url;

struct PreparedDiskUpdate {
    uri: Url,
    path: PathBuf,
    semantics: Option<PreparedSemanticSnapshot>,
    imports: Vec<(analysis::TextRange, PathBuf)>,
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
    ) -> Option<bool> {
        let (id, ticket, path) = self.begin_revision(&uri, path, Revision(version), true)?;
        tracing::Span::current().record("file_id", tracing::field::debug(&id));
        if !revision_result(ticket.wait_for_turn().await) {
            return None;
        }
        let build_path = path.clone();
        let opened = run_staging(self.workspace.staging.clone(), move || {
            let snapshot = Arc::new(trace_document_snapshot(Revision(version), text));
            let document = Document::with_snapshot(uri, language_id, snapshot);
            let needs_prelude = analysis::imports(&document)
                .iter()
                .any(|import| import.path_text(&document.text) == "Base");
            let imports = build_path.as_deref().map_or_else(Vec::new, |path| {
                crate::workspace::resolve_import_targets(path, &document.snapshot)
            });
            let semantics = prepare_semantic_snapshot(document.snapshot.clone());
            (document, imports, needs_prelude, semantics)
        })
        .await?;
        let (document, imports, needs_prelude, semantics) = opened;
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
                    Some(database.set_open_document_prepared(document, path, imports, semantics))
                })
            })?;
            commit_span.record("file_id", tracing::field::debug(&root));
            commit_span.record("outcome", "committed");
            root
        };
        self.load_reachable_async(std::slice::from_ref(&root)).await;
        if !self.workspace.finish_revision(&ticket) {
            return None;
        }
        tracing::Span::current().record("outcome", "committed");
        drop(ticket);
        Some(needs_prelude)
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

    pub(super) async fn workspace_read(&self) -> RwLockReadGuard<'_, ()> {
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
    ) -> Vec<(watch::Receiver<RevisionStatus>, u64)> {
        let state = self.workspace.state.read();
        let revisions = &state.revisions;
        // Most queries have no pending updates. Avoid walking the import graph
        // or allocating waiters on this warm path.
        if !revisions
            .values()
            .any(|sync| revision_result(sync.status()).is_pending())
        {
            return Vec::new();
        }
        let dependencies = if let Some(uri) = uri {
            let database = &state.database;
            let Some(root) = database.file_id_by_uri(uri) else {
                return Vec::new();
            };
            Some(database.dependencies(root))
        } else {
            None
        };
        revisions
            .iter()
            .filter_map(|(id, sync)| {
                let state = revision_result(sync.status());
                (state.is_pending()
                    && dependencies
                        .as_ref()
                        .is_none_or(|dependencies| dependencies.contains(id)))
                .then(|| (sync.subscribe(), state.generation()))
            })
            .collect()
    }

    pub(super) async fn ready_read(&self, uri: Option<&Url>) -> RwLockReadGuard<'_, ()> {
        loop {
            let read = self.workspace_read().await;
            let pending = self.pending_revisions(uri);
            if pending.is_empty() {
                return read;
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

    pub(super) fn document(&self, uri: &Url) -> Option<Document> {
        let database = self.workspace.read();
        let document = database.open_document(uri)?;
        if tracing::enabled!(
            target: "bend2_lsp::server::lsp",
            tracing::Level::INFO
        ) && let Some(id) = database.file_id_by_uri(uri)
        {
            let span = tracing::Span::current();
            span.record("file_id", tracing::field::debug(&id));
            span.record("revision", document.revision.0);
        }
        Some(document)
    }

    pub(super) fn cached_document(&self, uri: &Url) -> Option<Document> {
        self.workspace.read().cached_document(uri)
    }
    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "open_documents", result_count = tracing::field::Empty))]
    pub(super) fn workspace_documents(&self) -> Vec<Document> {
        let documents = self.workspace.read().open_documents();
        tracing::Span::current().record("result_count", documents.len());
        documents
    }
    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "indexed_documents", result_count = tracing::field::Empty))]
    pub(super) fn indexed_documents(&self) -> Vec<Document> {
        let documents = self.workspace.read().indexed_documents();
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
        let Some((document, path, imports, semantics)) =
            run_staging(self.workspace.staging.clone(), move || {
                let imports = path.as_deref().map_or_else(Vec::new, |path| {
                    crate::workspace::resolve_import_targets(path, &document.snapshot)
                });
                let semantics = prepare_semantic_snapshot(document.snapshot.clone());
                (document, path, imports, semantics)
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
                        Some(
                            database.set_open_document_prepared(document, path, imports, semantics),
                        )
                    })
            }) else {
                return;
            };
            commit_span.record("file_id", tracing::field::debug(&root));
            commit_span.record("outcome", "committed");
            root
        };
        tracing::Span::current().record("file_id", tracing::field::debug(&root));
        self.load_reachable_async(std::slice::from_ref(&root)).await;
        if self.workspace.finish_revision(&ticket) {
            tracing::Span::current().record("outcome", "committed");
        } else {
            tracing::Span::current().record("outcome", "superseded");
        }
    }

    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "load_reachable", root_count = roots.len(), outcome = tracing::field::Empty))]
    pub(super) async fn load_reachable_async(&self, roots: &[FileId]) {
        let roots = roots.to_vec();
        let mut attempted = HashSet::new();
        loop {
            let generation = self.workspace.generation();
            let paths = {
                let database = self.workspace.read();
                database
                    .missing_disk_paths(&roots)
                    .into_iter()
                    .filter(|path| !attempted.contains(path))
                    .collect::<Vec<_>>()
            };
            if paths.is_empty() {
                break;
            }
            let Some(prepared) = run_staging(self.workspace.staging.clone(), move || {
                paths
                    .into_iter()
                    .map(|path| {
                        let snapshot = std::fs::read_to_string(&path).ok().map(|text| {
                            Arc::new(trace_document_snapshot(Revision::UNVERSIONED, text))
                        });
                        let imports = snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
                            crate::workspace::resolve_import_targets(&path, snapshot)
                        });
                        let semantics = snapshot.map(prepare_semantic_snapshot);
                        (path, semantics, imports)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            else {
                return;
            };
            let _workspace_update = self.workspace.updates.write().await;
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "load_reachable",
                file_count = prepared.len(),
                outcome = tracing::field::Empty,
            );
            let committed = commit_span.in_scope(|| {
                self.workspace.commit(None, Some(generation), |database| {
                    for (path, snapshot, imports) in prepared {
                        if snapshot.is_none() {
                            attempted.insert(path.clone());
                        }
                        database.sync_disk_snapshot_prepared(&path, snapshot, imports);
                    }
                    Some(())
                })
            });
            if committed.is_none() {
                continue;
            }
            commit_span.record("outcome", "committed");
        }
        let generation = self.workspace.generation();
        let _workspace_update = self.workspace.updates.write().await;
        let commit_span = tracing::info_span!(
            "workspace.commit",
            kind = "finish_load_reachable",
            root_count = roots.len(),
            outcome = tracing::field::Empty,
        );
        if commit_span
            .in_scope(|| {
                self.workspace.commit(None, Some(generation), |database| {
                    database.finish_load_reachable(&roots);
                    Some(())
                })
            })
            .is_none()
        {
            return;
        }
        commit_span.record("outcome", "committed");
        tracing::Span::current().record("outcome", "complete");
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
        let Some(document) =
            revision_result(ticket.staged_document()).or_else(|| self.document(&uri))
        else {
            return false;
        };
        let changes = params.content_changes;
        let Some((updated_document, imports, semantics)) =
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
                let imports = path.as_deref().map_or_else(Vec::new, |path| {
                    crate::workspace::resolve_import_targets(path, &snapshot)
                });
                let semantics = prepare_semantic_snapshot(snapshot);
                (updated_document, imports, semantics)
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
                    let (_, imports_changed) =
                        database.update_open_snapshot_prepared(&uri, semantics, imports)?;
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
            self.load_reachable_async(std::slice::from_ref(&id)).await;
        }
        if !self.workspace.finish_revision(&ticket) {
            return false;
        }
        tracing::Span::current().record("outcome", "committed");
        true
    }
    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "close", file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    pub(super) async fn close_workspace_document(
        &self,
        uri: Url,
        closed: super::workspace_service::ClosedRevision,
    ) -> bool {
        let _serial = self.workspace.update_serial.lock().await;
        let id = loop {
            if !self.workspace.is_closed(closed) {
                return false;
            }
            let workspace = self.workspace.clone();
            let read_uri = uri.clone();
            let Some((imports, semantics)) =
                run_staging(self.workspace.staging.clone(), move || {
                    let disk = workspace.read().disk_document(&read_uri);
                    disk.map_or_else(
                        || (Vec::new(), None),
                        |(path, snapshot)| {
                            let imports =
                                crate::workspace::resolve_import_targets(&path, &snapshot);
                            (imports, Some(prepare_semantic_snapshot(snapshot)))
                        },
                    )
                })
                .await
            else {
                return false;
            };
            let _workspace_update = self.workspace.updates.write().await;
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "close",
                file_id = tracing::field::Empty,
                outcome = tracing::field::Empty,
            );
            match commit_span.in_scope(|| {
                self.workspace
                    .commit_close(closed, &uri, imports, semantics)
            }) {
                super::workspace_service::CloseCommit::Committed(id) => {
                    commit_span.record("file_id", tracing::field::debug(&id));
                    commit_span.record("outcome", "committed");
                    tracing::Span::current().record("file_id", tracing::field::debug(&id));
                    break id;
                }
                super::workspace_service::CloseCommit::RetryDisk => {
                    commit_span.record("outcome", "disk_changed");
                }
                super::workspace_service::CloseCommit::Superseded => {
                    commit_span.record("outcome", "superseded");
                    return false;
                }
            }
        };
        self.load_reachable_async(std::slice::from_ref(&id)).await;
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
            .find(|import| import.alias_text(&source.text) == Some(alias))?;
        let imported = import.path_text(&source.text);
        if imported == "Base" {
            let base = self.compiler.base_module.read().clone()?;
            return Some((base.uri, base.snapshot));
        }
        let document = self
            .workspace
            .read()
            .import_target(&source.uri, import.path)?;
        Some((document.uri, document.snapshot))
    }

    pub(super) fn prelude_module(&self, source: &Document) -> Option<BaseModule> {
        if !analysis::imports(source)
            .iter()
            .any(|import| import.path_text(&source.text) == "Base")
        {
            return None;
        }
        self.compiler.base_module.read().clone()
    }

    pub(super) fn prelude_declaration(&self, source: &Document, name: &str) -> Option<BaseModule> {
        let module = self.prelude_module(source)?;
        analysis::declaration_range(&module, name)?;
        Some(module)
    }

    pub(super) fn symbol_source(
        &self,
        document: &Document,
        name: &str,
    ) -> (Url, Arc<DocumentSnapshot>, String) {
        if let Some((alias, member)) = name.split_once('.')
            && let Some((uri, snapshot)) = self.module_document(document, alias)
        {
            return (uri, snapshot, member.to_owned());
        }
        (
            document.uri.clone(),
            document.snapshot.clone(),
            name.to_owned(),
        )
    }

    pub(super) fn hierarchy_source(
        &self,
        document: &Document,
        name: &str,
    ) -> Option<(Url, Arc<DocumentSnapshot>, String)> {
        let (uri, snapshot, member) = self.symbol_source(document, name);
        if named_document_symbol(&snapshot, &member).is_some() {
            return Some((uri, snapshot, member));
        }
        let module = self.prelude_module(document)?;
        named_document_symbol(&module, name)?;
        Some((module.uri, module.snapshot, name.to_owned()))
    }

    pub(super) fn hierarchy_document(&self, uri: &Url) -> Option<Document> {
        self.cached_document(uri)
    }

    #[tracing::instrument(name = "analysis.query", skip_all, fields(kind = "symbol_references", result_count = tracing::field::Empty))]
    pub(super) fn symbol_references(
        &self,
        target_uri: &Url,
        name: &str,
        include_declaration: bool,
    ) -> Vec<Location> {
        let database = self.workspace.read();
        let Some(target) = database.symbol_by_name(target_uri, name) else {
            tracing::Span::current().record("result_count", 0);
            return Vec::new();
        };
        let locations = reference_locations::symbol_reference_locations(
            database.references(target.id, include_declaration),
        );
        tracing::Span::current().record("result_count", locations.len());
        locations
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
                        database.dependents(id)
                    });
                    tracing::Span::current().record("result_count", documents.len());
                    documents
                })
            })
            .await,
        )
    }

    pub(super) fn watched_snapshot_context(&self, events: &[(Url, PathBuf)]) -> HashMap<Url, i32> {
        let database = self.workspace.read();
        let mut affected = HashMap::new();
        for (uri, path) in events {
            let Some(id) = database
                .file_id_by_uri(uri)
                .or_else(|| database.file_id_by_path(path))
            else {
                continue;
            };
            for document in database.dependents(id) {
                affected.insert(document.uri.clone(), document.revision.0);
            }
        }
        affected
    }

    async fn prepare_watched_updates(
        semaphore: Arc<Semaphore>,
        events: Vec<(Url, PathBuf)>,
    ) -> Option<Vec<PreparedDiskUpdate>> {
        run_staging(semaphore, move || {
            events
                .into_iter()
                .map(|(uri, path)| {
                    let snapshot = std::fs::read_to_string(&path)
                        .ok()
                        .map(|text| Arc::new(trace_document_snapshot(Revision::UNVERSIONED, text)));
                    let imports = snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
                        crate::workspace::resolve_import_targets(&path, snapshot)
                    });
                    let semantics = snapshot.map(prepare_semantic_snapshot);
                    PreparedDiskUpdate {
                        uri,
                        path,
                        semantics,
                        imports,
                    }
                })
                .collect()
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
        let mut affected = self.watched_snapshot_context(&events);
        let Some(prepared) =
            Self::prepare_watched_updates(self.workspace.staging.clone(), events).await
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
                    let mut updates = Vec::with_capacity(prepared.len());
                    for update in prepared {
                        let Some((id, imports_changed)) = database.sync_disk_snapshot_prepared(
                            &update.path,
                            update.semantics,
                            update.imports,
                        ) else {
                            continue;
                        };
                        if let Some(document) = database.open_document(&update.uri) {
                            affected.insert(document.uri.clone(), document.revision.0);
                        }
                        updates.push((id, imports_changed));
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
                self.load_reachable_async(std::slice::from_ref(id)).await;
            }
        }
        let workspace_db = self.workspace.clone();
        let Some(after) = run_staging(self.workspace.staging.clone(), move || {
            let database = workspace_db.read();
            let mut affected = HashMap::<Url, i32>::new();
            for (id, _) in updates {
                for document in database.dependents(id) {
                    affected.insert(document.uri.clone(), document.revision.0);
                }
            }
            affected
        })
        .await
        else {
            return HashMap::new();
        };
        affected.extend(after);
        tracing::Span::current().record("outcome", "committed");
        affected
    }
}
