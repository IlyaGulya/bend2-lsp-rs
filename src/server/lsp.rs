use super::capabilities::server_capabilities;
use super::compiler::{
    CachedCompilerResult, CompilerConfig, CompilerReapers, compiler_base, compiler_diagnostics,
};
use super::features::{
    call_hierarchy_item, code_end_offset, cursor_in_comment_or_string, lexical_diagnostics,
    named_document_symbol, static_hover, token_at, type_hierarchy_item, type_hierarchy_symbol,
};
use super::formatter::format_bend;
use crate::analysis::{self, DocumentSnapshot, LineIndex, Revision};
use crate::workspace::{Document, FileId, SourceGraph, WorkspaceDb, normalize_path};
use std::{
    collections::{HashMap, HashSet, hash_map::DefaultHasher},
    future::Future,
    hash::{Hash, Hasher},
    ops::Deref,
    path::PathBuf,
    sync::{
        Arc, Mutex as SyncMutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Mutex as AsyncMutex, Notify, RwLock as AsyncRwLock, RwLockReadGuard, Semaphore, watch},
    task::JoinHandle,
    time::sleep,
};
use tower_lsp::{
    Client, LanguageServer,
    jsonrpc::Result,
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
        CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
        CodeLens, CodeLensParams, CompletionItem, CompletionItemKind, CompletionParams,
        CompletionResponse, Diagnostic, DidChangeConfigurationParams, DidChangeTextDocumentParams,
        DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DocumentFormattingParams, DocumentHighlight,
        DocumentHighlightKind, DocumentHighlightParams, DocumentLink, DocumentLinkParams,
        DocumentOnTypeFormattingParams, DocumentRangeFormattingParams, DocumentSymbolParams,
        DocumentSymbolResponse, FoldingRange, FoldingRangeParams, GotoDefinitionParams,
        GotoDefinitionResponse, Hover, HoverContents, HoverParams, InitializeParams,
        InitializeResult, InitializedParams, InlayHint, InlayHintParams, Location, MarkupContent,
        MarkupKind, MessageType, NumberOrString, Position, Range, ReferenceParams, Registration,
        RenameParams, SelectionRange, SelectionRangeParams, SemanticTokens, SemanticTokensParams,
        SemanticTokensResult, ServerInfo, SignatureHelp, SignatureHelpParams, SymbolInformation,
        SymbolKind, TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams,
        TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, WorkspaceEdit,
        WorkspaceSymbolParams,
    },
};
use tracing::Instrument;
use url::Url;
#[derive(Clone, Copy)]
struct RevisionStatus {
    desired: Option<Revision>,
    committed: Option<Revision>,
    generation: u64,
    failed_generation: Option<u64>,
}

impl RevisionStatus {
    fn is_pending(self) -> bool {
        self.desired.is_some()
            && self.committed != self.desired
            && self.failed_generation != Some(self.generation)
    }
}

struct StageOrder {
    next_generation: u64,
    skipped: HashSet<u64>,
}

struct DocumentRevisionSync {
    status: watch::Sender<RevisionStatus>,
    stage_order: SyncMutex<StageOrder>,
    stage_ready: Notify,
    staged_document: SyncMutex<Option<Document>>,
}

impl DocumentRevisionSync {
    fn new(document: Option<Document>) -> Self {
        let revision = document.as_ref().map(|document| document.revision);
        let (status, _) = watch::channel(RevisionStatus {
            desired: revision,
            committed: revision,
            generation: 0,
            failed_generation: None,
        });
        Self {
            status,
            stage_order: SyncMutex::new(StageOrder {
                next_generation: 1,
                skipped: HashSet::new(),
            }),
            stage_ready: Notify::new(),
            staged_document: SyncMutex::new(document),
        }
    }

    fn reserve(self: &Arc<Self>, revision: Revision) -> Option<RevisionTicket> {
        let mut generation = None;
        self.status.send_if_modified(|status| {
            if status
                .desired
                .is_some_and(|desired| revision.0 <= desired.0)
            {
                return false;
            }
            let Some(next_generation) = status.generation.checked_add(1) else {
                return false;
            };
            status.desired = Some(revision);
            status.generation = next_generation;
            status.failed_generation = None;
            generation = Some(next_generation);
            true
        });
        Some(RevisionTicket {
            sync: self.clone(),
            revision,
            generation: generation?,
        })
    }
    fn reserve_refresh(self: &Arc<Self>, revision: Revision) -> Option<RevisionTicket> {
        let mut generation = None;
        self.status.send_if_modified(|status| {
            if status.desired != Some(revision) {
                return false;
            }
            let Some(next_generation) = status.generation.checked_add(1) else {
                return false;
            };
            status.generation = next_generation;
            status.committed = None;
            status.failed_generation = None;
            generation = Some(next_generation);
            true
        });
        Some(RevisionTicket {
            sync: self.clone(),
            revision,
            generation: generation?,
        })
    }

    fn finish_stage(&self, generation: u64) {
        if let Ok(mut order) = self.stage_order.lock() {
            if generation == order.next_generation {
                order.next_generation += 1;
                loop {
                    let next_generation = order.next_generation;
                    if !order.skipped.remove(&next_generation) {
                        break;
                    }
                    order.next_generation += 1;
                }
            } else if generation > order.next_generation {
                order.skipped.insert(generation);
            }
        }
        self.stage_ready.notify_waiters();
    }
}
async fn wait_for_captured_revision(
    mut status: watch::Receiver<RevisionStatus>,
    captured_generation: u64,
) {
    loop {
        let state = *status.borrow_and_update();
        if state.generation != captured_generation
            || state.desired.is_none()
            || state.committed == state.desired
            || state.failed_generation == Some(captured_generation)
        {
            return;
        }
        if status.changed().await.is_err() {
            return;
        }
    }
}

struct RevisionTicket {
    sync: Arc<DocumentRevisionSync>,
    revision: Revision,
    generation: u64,
}

impl RevisionTicket {
    async fn wait_for_turn(&self) {
        loop {
            let notified = self.sync.stage_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .sync
                .stage_order
                .lock()
                .is_ok_and(|order| order.next_generation == self.generation)
            {
                return;
            }
            notified.await;
        }
    }

    fn is_current(&self) -> bool {
        let status = *self.sync.status.borrow();
        status.generation == self.generation && status.desired == Some(self.revision)
    }

    fn store_staged_document(&self, document: Document) {
        if let Ok(mut staged) = self.sync.staged_document.lock() {
            *staged = Some(document);
        }
    }

    fn staged_document(&self) -> Option<Document> {
        self.sync
            .staged_document
            .lock()
            .ok()
            .and_then(|document| document.clone())
    }

    fn mark_committed(&self) -> bool {
        self.sync.status.send_if_modified(|status| {
            if status.generation != self.generation || status.desired != Some(self.revision) {
                return false;
            }
            status.committed = Some(self.revision);
            status.failed_generation = None;
            true
        })
    }
}

impl Drop for RevisionTicket {
    fn drop(&mut self) {
        self.sync.status.send_if_modified(|status| {
            if status.generation == self.generation
                && status.desired == Some(self.revision)
                && status.committed != Some(self.revision)
            {
                status.failed_generation = Some(self.generation);
                true
            } else {
                false
            }
        });
        self.sync.finish_stage(self.generation);
    }
}

type RevisionSyncs = Arc<RwLock<HashMap<FileId, Arc<DocumentRevisionSync>>>>;

#[derive(Clone)]
struct ImportedDiagnostics {
    version: i32,
    by_uri: HashMap<Url, Vec<Diagnostic>>,
}

fn diagnostics_revision_is_current(current: Option<Revision>, result: Revision) -> bool {
    current == Some(result)
}

async fn publish_if_current<T, F, Fut>(
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

fn replace_imported_diagnostics_if_current(
    snapshots: &mut HashMap<Url, ImportedDiagnostics>,
    root_uri: Url,
    version: i32,
    by_uri: HashMap<Url, Vec<Diagnostic>>,
) -> Option<HashSet<Url>> {
    if snapshots
        .get(&root_uri)
        .is_some_and(|previous| previous.version > version)
    {
        return None;
    }
    let mut affected = HashSet::new();
    if let Some(previous) = snapshots.get(&root_uri) {
        affected.extend(previous.by_uri.keys().cloned());
    }
    affected.extend(by_uri.keys().cloned());
    snapshots.insert(root_uri, ImportedDiagnostics { version, by_uri });
    Some(affected)
}
struct PreparedDiskUpdate {
    uri: Url,
    path: PathBuf,
    snapshot: Option<Arc<DocumentSnapshot>>,
    imports: Vec<(analysis::TextRange, PathBuf)>,
}

#[derive(Clone)]
struct BaseModule {
    uri: Url,
    snapshot: Arc<DocumentSnapshot>,
}

impl Deref for BaseModule {
    type Target = DocumentSnapshot;

    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}
pub(super) type AnalysisTasks = Arc<AsyncMutex<HashMap<Url, (u64, JoinHandle<()>)>>>;
async fn run_staging<T, F>(semaphore: Arc<Semaphore>, operation: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let span = tracing::Span::current();
    let permit = semaphore.acquire_owned().await.ok()?;
    tokio::task::spawn_blocking(move || {
        span.in_scope(|| {
            let _permit = permit;
            operation()
        })
    })
    .await
    .ok()
}

fn trace_document_snapshot(revision: Revision, text: String) -> DocumentSnapshot {
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

fn trace_document_snapshot_with_line_index(
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

fn record_snapshot_counts(span: &tracing::Span, snapshot: &DocumentSnapshot) {
    if span.is_disabled() {
        return;
    }
    span.record("token_count", snapshot.syntax.tokens().len());
    span.record("import_count", snapshot.syntax.imports().len());
    span.record("symbol_count", snapshot.syntax.symbols().len());
    span.record("call_count", snapshot.syntax.calls().len());
}

#[derive(Clone)]
pub(super) struct Backend {
    client: Client,
    workspace_update_serial: Arc<AsyncMutex<()>>,
    workspace_generation: Arc<AtomicU64>,
    workspace_updates: Arc<AsyncRwLock<()>>,
    revision_syncs: RevisionSyncs,
    workspace_db: Arc<RwLock<WorkspaceDb>>,
    imported_diagnostics: Arc<RwLock<HashMap<Url, ImportedDiagnostics>>>,
    imported_diagnostic_publish: Arc<AsyncMutex<()>>,
    workspace_roots: Arc<RwLock<Vec<PathBuf>>>,
    watch_registration: Arc<RwLock<bool>>,
    type_hierarchy_registration: Arc<RwLock<bool>>,
    compiler_config: Arc<RwLock<CompilerConfig>>,
    base_module: Arc<RwLock<Option<BaseModule>>>,
    base_module_attempted: Arc<RwLock<bool>>,
    base_module_load: Arc<AsyncMutex<()>>,
    analysis_tasks: AnalysisTasks,
    next_analysis_id: Arc<AtomicU64>,
    analysis_semaphore: Arc<Semaphore>,
    compiler_semaphore: Arc<Semaphore>,
    staging_semaphore: Arc<Semaphore>,
    compiler_results: Arc<RwLock<HashMap<PathBuf, CachedCompilerResult>>>,
    compiler_reapers: Arc<CompilerReapers>,
}

impl Backend {
    pub(super) fn new(
        client: Client,
        analysis_tasks: AnalysisTasks,
        compiler_reapers: Arc<CompilerReapers>,
    ) -> Self {
        Self {
            client,
            workspace_update_serial: Arc::new(AsyncMutex::new(())),
            workspace_generation: Arc::new(AtomicU64::new(0)),
            workspace_updates: Arc::new(AsyncRwLock::new(())),
            revision_syncs: Arc::new(RwLock::new(HashMap::new())),
            workspace_db: Arc::new(RwLock::new(WorkspaceDb::default())),
            imported_diagnostics: Arc::new(RwLock::new(HashMap::new())),
            imported_diagnostic_publish: Arc::new(AsyncMutex::new(())),
            workspace_roots: Arc::new(RwLock::new(Vec::new())),
            watch_registration: Arc::new(RwLock::new(false)),
            type_hierarchy_registration: Arc::new(RwLock::new(false)),
            compiler_config: Arc::new(RwLock::new(CompilerConfig::default())),
            base_module: Arc::new(RwLock::new(None)),
            base_module_attempted: Arc::new(RwLock::new(false)),
            base_module_load: Arc::new(AsyncMutex::new(())),
            analysis_tasks,
            next_analysis_id: Arc::new(AtomicU64::new(1)),
            analysis_semaphore: Arc::new(Semaphore::new(4)),
            compiler_semaphore: Arc::new(Semaphore::new(4)),
            staging_semaphore: Arc::new(Semaphore::new(4)),
            compiler_results: Arc::new(RwLock::new(HashMap::new())),
            compiler_reapers,
        }
    }
    fn invalidate_document_revision(&self, uri: &Url) {
        let Ok(mut revisions) = self.revision_syncs.write() else {
            return;
        };
        let Ok(database) = self.workspace_db.read() else {
            return;
        };
        let Some(id) = database.file_id_by_uri(uri) else {
            return;
        };
        let Some(sync) = revisions.get_mut(&id) else {
            return;
        };
        sync.status.send_modify(|status| {
            status.desired = None;
            status.committed = None;
            status.failed_generation = None;
        });
        if let Ok(mut staged) = sync.staged_document.lock() {
            *staged = None;
        }
    }
    fn begin_revision(
        &self,
        uri: &Url,
        path: Option<PathBuf>,
        revision: Revision,
        create: bool,
    ) -> Option<(FileId, RevisionTicket, Option<PathBuf>)> {
        let mut revisions = self.revision_syncs.write().ok()?;
        let mut database = self.workspace_db.write().ok()?;
        let id = if create {
            database.ensure_file_id(uri.clone(), path)
        } else {
            database.file_id_by_uri(uri)?
        };
        let document = database.open_document(uri);
        let file_path = database.document_path(uri);
        let sync = revisions
            .entry(id)
            .or_insert_with(|| Arc::new(DocumentRevisionSync::new(document)))
            .clone();
        let ticket = sync.reserve(revision)?;
        Some((id, ticket, file_path))
    }
    fn begin_document_refresh(&self, uri: &Url, revision: Revision) -> Option<RevisionTicket> {
        let revisions = self.revision_syncs.write().ok()?;
        let database = self.workspace_db.read().ok()?;
        let id = database.file_id_by_uri(uri)?;
        revisions.get(&id)?.reserve_refresh(revision)
    }

    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "open", revision = version, source_bytes = text.len(), file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    async fn open_workspace_text(
        &self,
        uri: Url,
        language_id: String,
        version: i32,
        text: String,
        path: Option<PathBuf>,
    ) -> Option<bool> {
        let (id, ticket, path) = self.begin_revision(&uri, path, Revision(version), true)?;
        tracing::Span::current().record("file_id", tracing::field::debug(&id));
        ticket.wait_for_turn().await;
        let build_path = path.clone();
        let opened = run_staging(self.staging_semaphore.clone(), move || {
            let snapshot = Arc::new(trace_document_snapshot(Revision(version), text));
            let document = Document::with_snapshot(uri, language_id, snapshot);
            let needs_prelude = analysis::imports(&document)
                .iter()
                .any(|import| import.path_text(&document.text) == "Base");
            let imports = build_path.as_deref().map_or_else(Vec::new, |path| {
                crate::workspace::resolve_import_targets(path, &document.snapshot)
            });
            (document, imports, needs_prelude)
        })
        .await?;
        let (document, imports, needs_prelude) = opened;
        ticket.store_staged_document(document.clone());
        if !ticket.is_current() {
            return None;
        }
        let root = {
            let _workspace_update = self.workspace_updates.write().await;
            let _revisions = self.revision_syncs.read().ok()?;
            if !ticket.is_current() {
                return None;
            }
            let Ok(mut database) = self.workspace_db.write() else {
                return None;
            };
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "open",
                file_count = 1,
                file_id = tracing::field::Empty,
                outcome = tracing::field::Empty,
            );
            let root = commit_span.in_scope(|| {
                let root = database.set_open_document_prepared(document, path, imports);
                self.workspace_generation.fetch_add(1, Ordering::Release);
                root
            });
            commit_span.record("file_id", tracing::field::debug(&root));
            commit_span.record("outcome", "committed");
            root
        };
        self.load_reachable_async(std::slice::from_ref(&root)).await;
        if !ticket.mark_committed() {
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
    async fn wait_for_document_revision(&self, uri: &Url) {
        let span = tracing::Span::current();
        let sync = {
            let Ok(revisions) = self.revision_syncs.read() else {
                span.record("outcome", "unavailable");
                return;
            };
            let Ok(database) = self.workspace_db.read() else {
                span.record("outcome", "unavailable");
                return;
            };
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
        let mut status = sync.status.subscribe();
        loop {
            let state = *status.borrow_and_update();
            span.record(
                "desired_revision",
                state.desired.map_or(-1, |revision| revision.0),
            );
            span.record(
                "committed_revision",
                state.committed.map_or(-1, |revision| revision.0),
            );
            if state.desired.is_none() {
                span.record("outcome", "closed");
                return;
            }
            if state.committed == state.desired {
                span.record("outcome", "ready");
                return;
            }
            if state.failed_generation == Some(state.generation) {
                span.record("outcome", "failed");
                return;
            }
            if status.changed().await.is_err() {
                span.record("outcome", "closed");
                return;
            }
        }
    }

    async fn document_read(&self, uri: &Url) -> RwLockReadGuard<'_, ()> {
        self.wait_for_document_revision(uri).await;
        self.ready_read(Some(uri)).await
    }

    async fn workspace_read(&self) -> RwLockReadGuard<'_, ()> {
        // Cold snapshot construction never holds this committed-view guard.
        self.workspace_updates
            .read()
            .instrument(tracing::info_span!("workspace.sync_wait"))
            .await
    }
    fn pending_revisions(&self, uri: Option<&Url>) -> Vec<(watch::Receiver<RevisionStatus>, u64)> {
        let Ok(revisions) = self.revision_syncs.read() else {
            return Vec::new();
        };
        // Most queries have no pending updates. Avoid walking the import graph
        // or allocating waiters on this warm path.
        if !revisions
            .values()
            .any(|sync| sync.status.borrow().is_pending())
        {
            return Vec::new();
        }
        let dependencies = if let Some(uri) = uri {
            let Ok(database) = self.workspace_db.read() else {
                return Vec::new();
            };
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
                let state = *sync.status.borrow();
                (state.is_pending()
                    && dependencies
                        .as_ref()
                        .is_none_or(|dependencies| dependencies.contains(id)))
                .then(|| (sync.status.subscribe(), state.generation))
            })
            .collect()
    }

    async fn ready_read(&self, uri: Option<&Url>) -> RwLockReadGuard<'_, ()> {
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
                    wait_for_captured_revision(status, generation).await;
                }
            }
            .instrument(span)
            .await;
            // A waited-for generation can be superseded, or its imports can
            // change. Recheck relevant revisions under the committed-view guard.
        }
    }

    async fn workspace_ready_read(&self) -> RwLockReadGuard<'_, ()> {
        self.ready_read(None).await
    }

    fn document(&self, uri: &Url) -> Option<Document> {
        let database = self.workspace_db.read().ok()?;
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

    fn cached_document(&self, uri: &Url) -> Option<Document> {
        self.workspace_db.read().ok()?.cached_document(uri)
    }
    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "open_documents", result_count = tracing::field::Empty))]
    fn workspace_documents(&self) -> Vec<Document> {
        let documents = self
            .workspace_db
            .read()
            .map(|database| database.open_documents())
            .unwrap_or_default();
        tracing::Span::current().record("result_count", documents.len());
        documents
    }
    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "indexed_documents", result_count = tracing::field::Empty))]
    fn indexed_documents(&self) -> Vec<Document> {
        let documents = self
            .workspace_db
            .read()
            .map(|database| database.indexed_documents())
            .unwrap_or_default();
        tracing::Span::current().record("result_count", documents.len());
        documents
    }
    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "refresh", revision = document.revision.0, source_bytes = document.text.len(), file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    async fn open_workspace_document(&self, document: Document, path: Option<PathBuf>) {
        let Some(ticket) = self.begin_document_refresh(&document.uri, document.revision) else {
            return;
        };
        ticket.wait_for_turn().await;
        let _serial = self.workspace_update_serial.lock().await;
        let generation = self.workspace_generation.load(Ordering::Acquire);
        let Some((document, path, imports)) =
            run_staging(self.staging_semaphore.clone(), move || {
                let imports = path.as_deref().map_or_else(Vec::new, |path| {
                    crate::workspace::resolve_import_targets(path, &document.snapshot)
                });
                (document, path, imports)
            })
            .await
        else {
            return;
        };
        let root = {
            let _workspace_update = self.workspace_updates.write().await;
            if self.workspace_generation.load(Ordering::Acquire) != generation {
                return;
            }
            let Ok(_revisions) = self.revision_syncs.read() else {
                return;
            };
            if !ticket.is_current() {
                return;
            }
            let Ok(mut database) = self.workspace_db.write() else {
                return;
            };
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "refresh",
                file_count = 1,
                file_id = tracing::field::Empty,
                outcome = tracing::field::Empty,
            );
            let root = commit_span.in_scope(|| {
                let root = database.set_open_document_prepared(document, path, imports);
                self.workspace_generation.fetch_add(1, Ordering::Release);
                root
            });
            commit_span.record("file_id", tracing::field::debug(&root));
            commit_span.record("outcome", "committed");
            root
        };
        tracing::Span::current().record("file_id", tracing::field::debug(&root));
        self.load_reachable_async(std::slice::from_ref(&root)).await;
        if ticket.mark_committed() {
            tracing::Span::current().record("outcome", "committed");
        } else {
            tracing::Span::current().record("outcome", "superseded");
        }
    }

    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "load_reachable", root_count = roots.len(), outcome = tracing::field::Empty))]
    async fn load_reachable_async(&self, roots: &[FileId]) {
        let roots = roots.to_vec();
        let mut attempted = HashSet::new();
        loop {
            let generation = self.workspace_generation.load(Ordering::Acquire);
            let paths = {
                let Ok(database) = self.workspace_db.read() else {
                    return;
                };
                database
                    .missing_disk_paths(&roots)
                    .into_iter()
                    .filter(|path| !attempted.contains(path))
                    .collect::<Vec<_>>()
            };
            if paths.is_empty() {
                break;
            }
            let Some(prepared) = run_staging(self.staging_semaphore.clone(), move || {
                paths
                    .into_iter()
                    .map(|path| {
                        let snapshot = std::fs::read_to_string(&path).ok().map(|text| {
                            Arc::new(trace_document_snapshot(Revision::UNVERSIONED, text))
                        });
                        let imports = snapshot.as_ref().map_or_else(Vec::new, |snapshot| {
                            crate::workspace::resolve_import_targets(&path, snapshot)
                        });
                        (path, snapshot, imports)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            else {
                return;
            };
            let _workspace_update = self.workspace_updates.write().await;
            if self.workspace_generation.load(Ordering::Acquire) != generation {
                continue;
            }
            let Ok(mut database) = self.workspace_db.write() else {
                return;
            };
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "load_reachable",
                file_count = prepared.len(),
                outcome = tracing::field::Empty,
            );
            commit_span.in_scope(|| {
                for (path, snapshot, imports) in prepared {
                    if snapshot.is_none() {
                        attempted.insert(path.clone());
                    }
                    database.sync_disk_snapshot_prepared(&path, snapshot, imports);
                }
                self.workspace_generation.fetch_add(1, Ordering::Release);
            });
            commit_span.record("outcome", "committed");
        }
        let generation = self.workspace_generation.load(Ordering::Acquire);
        let _workspace_update = self.workspace_updates.write().await;
        if self.workspace_generation.load(Ordering::Acquire) != generation {
            return;
        }
        if let Ok(mut database) = self.workspace_db.write() {
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "finish_load_reachable",
                root_count = roots.len(),
                outcome = tracing::field::Empty,
            );
            commit_span.in_scope(|| {
                database.finish_load_reachable(&roots);
                self.workspace_generation.fetch_add(1, Ordering::Release);
            });
            commit_span.record("outcome", "committed");
        }
        tracing::Span::current().record("outcome", "complete");
    }

    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "revision", revision = params.text_document.version, file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    async fn change_workspace_document(&self, params: DidChangeTextDocumentParams) -> bool {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        let Some((id, ticket, path)) = self.begin_revision(&uri, None, Revision(version), false)
        else {
            return false;
        };
        ticket.wait_for_turn().await;
        let Some(document) = ticket.staged_document().or_else(|| self.document(&uri)) else {
            return false;
        };
        let changes = params.content_changes;
        let Some((updated_document, imports)) =
            run_staging(self.staging_semaphore.clone(), move || {
                let mut text = if changes.first().is_some_and(|change| change.range.is_none()) {
                    String::new()
                } else {
                    document.text.clone()
                };
                let mut line_index = None;
                for change in changes {
                    if let Some(range) = change.range {
                        let current_index = line_index.as_ref().unwrap_or(&document.line_index);
                        let start =
                            current_index.offset(&text, range.start.line, range.start.character);
                        let end = current_index.offset(&text, range.end.line, range.end.character);
                        if start <= end {
                            text.replace_range(start..end, &change.text);
                        }
                    } else {
                        text = change.text;
                    }
                    line_index = Some(LineIndex::new(&text));
                }
                let snapshot = Arc::new(trace_document_snapshot_with_line_index(
                    Revision(version),
                    text,
                    line_index.unwrap_or_else(|| document.line_index.clone()),
                ));
                let updated_document = Document::with_snapshot(
                    document.uri.clone(),
                    document.language_id.clone(),
                    snapshot.clone(),
                );
                let imports = path.as_deref().map_or_else(Vec::new, |path| {
                    crate::workspace::resolve_import_targets(path, &snapshot)
                });
                (updated_document, imports)
            })
            .await
        else {
            return false;
        };
        ticket.store_staged_document(updated_document.clone());
        if !ticket.is_current() {
            return false;
        }
        let imports_changed = {
            let _workspace_update = self.workspace_updates.write().await;
            let Ok(_revisions) = self.revision_syncs.read() else {
                return false;
            };
            if !ticket.is_current() {
                return false;
            }
            let Ok(mut database) = self.workspace_db.write() else {
                return false;
            };
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "revision",
                file_id = tracing::field::debug(&id),
                outcome = tracing::field::Empty,
                imports_changed = tracing::field::Empty,
            );
            let result = commit_span.in_scope(|| {
                let (_, imports_changed) = database.update_open_snapshot_prepared(
                    &uri,
                    updated_document.snapshot,
                    imports,
                )?;
                self.workspace_generation.fetch_add(1, Ordering::Release);
                Some(imports_changed)
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
        if !ticket.mark_committed() {
            return false;
        }
        tracing::Span::current().record("outcome", "committed");
        true
    }
    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "close", file_id = tracing::field::Empty, outcome = tracing::field::Empty))]
    async fn close_workspace_document(&self, uri: Url) {
        self.invalidate_document_revision(&uri);
        let _serial = self.workspace_update_serial.lock().await;
        let generation = self.workspace_generation.load(Ordering::Acquire);
        let workspace_db = self.workspace_db.clone();
        let read_uri = uri.clone();
        let Some(imports) = run_staging(self.staging_semaphore.clone(), move || {
            let disk = {
                let Ok(database) = workspace_db.read() else {
                    return None;
                };
                database.disk_document(&read_uri)
            };
            Some(disk.map_or_else(Vec::new, |(path, snapshot)| {
                crate::workspace::resolve_import_targets(&path, &snapshot)
            }))
        })
        .await
        else {
            return;
        };
        let Some(imports) = imports else {
            return;
        };
        let id = {
            let _workspace_update = self.workspace_updates.write().await;
            if self.workspace_generation.load(Ordering::Acquire) != generation {
                return;
            }
            let Ok(mut database) = self.workspace_db.write() else {
                return;
            };
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "close",
                file_id = tracing::field::Empty,
                outcome = tracing::field::Empty,
            );
            let Some(id) = commit_span.in_scope(|| database.close_document_prepared(&uri, imports))
            else {
                return;
            };
            self.workspace_generation.fetch_add(1, Ordering::Release);
            commit_span.record("file_id", tracing::field::debug(&id));
            commit_span.record("outcome", "committed");
            tracing::Span::current().record("file_id", tracing::field::debug(&id));
            id
        };
        self.load_reachable_async(std::slice::from_ref(&id)).await;
        tracing::Span::current().record("outcome", "committed");
    }

    fn document_path(&self, uri: &Url) -> Option<PathBuf> {
        uri.to_file_path()
            .ok()
            .or_else(|| self.virtual_document_path(uri))
    }

    fn virtual_document_path(&self, uri: &Url) -> Option<PathBuf> {
        if uri.scheme() != "untitled" {
            return None;
        }
        let root = self.workspace_roots.read().ok()?.first()?.clone();
        let mut hasher = DefaultHasher::new();
        uri.hash(&mut hasher);
        Some(root.join(format!(".bend2-lsp-virtual-{:016x}.bend", hasher.finish())))
    }

    fn module_document(
        &self,
        source: &Document,
        alias: &str,
    ) -> Option<(Url, Arc<DocumentSnapshot>)> {
        let import = analysis::imports(source)
            .iter()
            .find(|import| import.alias_text(&source.text) == Some(alias))?;
        let imported = import.path_text(&source.text);
        if imported == "Base" {
            let base = self.base_module.read().ok()?.clone()?;
            return Some((base.uri, base.snapshot));
        }
        let document = self
            .workspace_db
            .read()
            .ok()?
            .import_target(&source.uri, import.path)?;
        Some((document.uri, document.snapshot))
    }

    fn prelude_module(&self, source: &Document) -> Option<BaseModule> {
        if !analysis::imports(source)
            .iter()
            .any(|import| import.path_text(&source.text) == "Base")
        {
            return None;
        }
        self.base_module.read().ok()?.clone()
    }

    fn prelude_declaration(&self, source: &Document, name: &str) -> Option<BaseModule> {
        let module = self.prelude_module(source)?;
        analysis::declaration_range(&module, name)?;
        Some(module)
    }

    async fn load_prelude_module(&self) {
        if self.base_module.read().is_ok_and(|module| module.is_some())
            || self
                .base_module_attempted
                .read()
                .is_ok_and(|attempted| *attempted)
        {
            return;
        }
        let _load = self.base_module_load.lock().await;
        if self.base_module.read().is_ok_and(|module| module.is_some())
            || self
                .base_module_attempted
                .read()
                .is_ok_and(|attempted| *attempted)
        {
            return;
        }
        let config = self
            .compiler_config
            .read()
            .map(|config| config.clone())
            .unwrap_or_default();
        let output = compiler_base(&config, self.compiler_reapers.clone()).await;
        let source = output
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .filter(|source| !source.is_empty());
        if let Some(source) = source {
            let loaded = run_staging(self.staging_semaphore.clone(), move || {
                let directory = tempfile::tempdir().ok()?;
                let path = directory.path().join("Base.bend");
                std::fs::write(&path, &source).ok()?;
                let uri = Url::from_file_path(&path).ok()?;
                Some((
                    BaseModule {
                        uri,
                        snapshot: Arc::new(DocumentSnapshot::new(Revision::UNVERSIONED, source)),
                    },
                    path,
                    directory,
                ))
            })
            .await
            .flatten();
            if let Some((module, path, directory)) = loaded
                && let Ok(mut database) = self.workspace_db.write()
                && let Ok(mut current) = self.base_module.write()
            {
                database.register_compiler_document(
                    module.uri.clone(),
                    path,
                    module.snapshot.clone(),
                    directory,
                );
                *current = Some(module);
            }
        }
        if let Ok(mut attempted) = self.base_module_attempted.write() {
            *attempted = true;
        }
    }
    async fn ensure_prelude_module(&self, source: &Document) {
        if analysis::imports(source)
            .iter()
            .any(|import| import.path_text(&source.text) == "Base")
        {
            self.load_prelude_module().await;
        }
    }

    fn symbol_source(
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

    fn resolve_call_target(
        &self,
        source: &Document,
        call: &analysis::CallSite,
    ) -> Option<(Url, Arc<DocumentSnapshot>, analysis::SymbolId)> {
        if let Some(callee) = call.callee {
            let symbol = source.syntax.symbol_by_id(callee)?;
            return (symbol.kind == analysis::SymbolKind::Function)
                .then(|| (source.uri.clone(), source.snapshot.clone(), callee));
        }
        if let Some(qualifier) = call.qualifier {
            if call
                .qualifier_token
                .is_some_and(|token| source.syntax.symbol_for_token(token).is_some())
            {
                return None;
            }
            let alias = source.syntax.name_text(&source.text, qualifier);
            let imported = self.module_document(source, alias).or_else(|| {
                (alias == "Base")
                    .then(|| self.prelude_module(source))
                    .flatten()
                    .map(|module| (module.uri, module.snapshot))
            })?;
            let name = source.syntax.name_text(&source.text, call.name);
            let id = imported
                .1
                .syntax
                .name_id(&imported.1.text, name)
                .and_then(|name| imported.1.syntax.symbol_by_name(name))
                .filter(|symbol| symbol.kind == analysis::SymbolKind::Function)?
                .id;
            return Some((imported.0, imported.1, id));
        }
        let module = self.prelude_module(source)?;
        let name = source.syntax.name_text(&source.text, call.name);
        let id = module
            .snapshot
            .syntax
            .name_id(&module.snapshot.text, name)
            .and_then(|name| module.snapshot.syntax.symbol_by_name(name))
            .filter(|symbol| symbol.kind == analysis::SymbolKind::Function)?
            .id;
        Some((module.uri, module.snapshot, id))
    }

    fn hierarchy_source(
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

    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "hierarchy_documents", result_count = tracing::field::Empty))]
    fn hierarchy_documents(&self) -> Vec<Document> {
        let mut documents = self.indexed_documents();
        if let Some(module) = self
            .base_module
            .read()
            .ok()
            .and_then(|module| module.clone())
            && !documents.iter().any(|document| document.uri == module.uri)
        {
            documents.push(Document::with_snapshot(
                module.uri,
                "bend".into(),
                module.snapshot,
            ));
        }
        tracing::Span::current().record("result_count", documents.len());
        documents
    }

    fn hierarchy_document(&self, uri: &Url) -> Option<Document> {
        self.cached_document(uri)
    }

    #[tracing::instrument(name = "analysis.query", skip_all, fields(kind = "symbol_references", result_count = tracing::field::Empty))]
    fn symbol_references(
        &self,
        target_uri: &Url,
        name: &str,
        include_declaration: bool,
    ) -> Vec<Location> {
        let span = tracing::Span::current();
        let target_path = self
            .document_path(target_uri)
            .map(|path| normalize_path(&path));
        let target_snapshot = self
            .cached_document(target_uri)
            .map(|document| document.snapshot);
        let Some(target_snapshot) = target_snapshot else {
            span.record("result_count", 0);
            return Vec::new();
        };
        let Some(target_name_id) = target_snapshot.syntax.name_id(&target_snapshot.text, name)
        else {
            span.record("result_count", 0);
            return Vec::new();
        };
        let Some(target_symbol) = target_snapshot.syntax.symbol_by_name(target_name_id) else {
            span.record("result_count", 0);
            return Vec::new();
        };
        let target_name = target_snapshot
            .syntax
            .name_text(&target_snapshot.text, target_symbol.name)
            .to_owned();
        let mut locations = Vec::new();
        for candidate in self.indexed_documents() {
            if !Self::supported(&candidate) {
                continue;
            }
            let candidate_path = self
                .document_path(&candidate.uri)
                .map(|path| normalize_path(&path));
            let same_target = candidate.uri == *target_uri
                || target_path
                    .as_ref()
                    .is_some_and(|path| candidate_path.as_ref() == Some(path));
            if same_target {
                let Some(candidate_name) = candidate
                    .syntax
                    .name_id(&candidate.text, &target_name)
                    .and_then(|name| candidate.syntax.symbol_by_name(name))
                else {
                    continue;
                };
                for reference in candidate.syntax.references(candidate_name.id) {
                    if include_declaration || reference.kind != analysis::ReferenceKind::Declaration
                    {
                        locations.push(Location {
                            uri: candidate.uri.clone(),
                            range: super::adapters::range(&candidate, reference.range),
                        });
                    }
                }
                continue;
            }
            let Some(candidate_name) = candidate.syntax.name_id(&candidate.text, &target_name)
            else {
                continue;
            };
            for reference in candidate.syntax.references_named(candidate_name) {
                let Some(qualifier) = reference.qualifier else {
                    continue;
                };
                if reference.resolved.is_some()
                    || reference
                        .qualifier_token
                        .is_some_and(|token| candidate.syntax.symbol_for_token(token).is_some())
                {
                    continue;
                }
                let alias = candidate.syntax.name_text(&candidate.text, qualifier);
                let Some((module_uri, _)) = self.module_document(&candidate, alias) else {
                    continue;
                };
                let module_path = self
                    .document_path(&module_uri)
                    .map(|path| normalize_path(&path));
                if module_path != target_path {
                    continue;
                }
                locations.push(Location {
                    uri: candidate.uri.clone(),
                    range: super::adapters::range(&candidate, reference.range),
                });
            }
        }
        locations.sort_by_key(|location| {
            (
                location.uri.to_string(),
                location.range.start.line,
                location.range.start.character,
            )
        });
        locations.dedup_by(|left, right| left.uri == right.uri && left.range == right.range);
        span.record("result_count", locations.len());
        locations
    }

    fn supported(doc: &Document) -> bool {
        doc.language_id == "bend" || doc.language_id == "bend2"
    }
    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "source_graph", file_id = tracing::field::Empty))]
    async fn compiler_source_graph(&self, uri: Url) -> Option<SourceGraph> {
        let span = tracing::Span::current();
        let permit = self.analysis_semaphore.clone().acquire_owned().await.ok()?;
        let workspace_db = self.workspace_db.clone();
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let _permit = permit;
                let database = workspace_db.read().ok()?;
                let root = database.file_id_by_uri(&uri)?;
                tracing::Span::current().record("file_id", tracing::field::debug(&root));
                database.source_graph(root)
            })
        })
        .await
        .ok()
        .flatten()
    }

    #[tracing::instrument(name = "workspace.query", skip_all, fields(kind = "dependents", file_id = tracing::field::Empty, result_count = tracing::field::Empty))]
    async fn dependent_documents(&self, changed: &Url) -> Vec<Document> {
        let span = tracing::Span::current();
        let Ok(permit) = self.analysis_semaphore.clone().acquire_owned().await else {
            return Vec::new();
        };
        let workspace_db = self.workspace_db.clone();
        let uri = changed.clone();
        let path = self.document_path(changed);
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let _permit = permit;
                let Ok(database) = workspace_db.read() else {
                    return Vec::new();
                };
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
        .await
        .unwrap_or_default()
    }
    async fn schedule_diagnostics(&self, uri: Url, version: i32) {
        let mut tasks = self.analysis_tasks.lock().await;
        if let Some((_, previous)) = tasks.get_mut(&uri) {
            previous.abort();
            let _ = previous.await;
        }
        let generation = self.next_analysis_id.fetch_add(1, Ordering::Relaxed);
        let backend = self.clone();
        let task_uri = uri.clone();
        let span = tracing::info_span!(
            parent: None,
            "diagnostics.run",
            revision = version,
            diagnostic_count = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        let handle = tokio::spawn(
            async move {
                backend.run_diagnostics(task_uri.clone(), version).await;
                let mut tasks = backend.analysis_tasks.lock().await;
                if tasks
                    .get(&task_uri)
                    .is_some_and(|(current, _)| *current == generation)
                {
                    tasks.remove(&task_uri);
                }
            }
            .instrument(span),
        );
        tasks.insert(uri, (generation, handle));
    }

    async fn cancel_diagnostics(&self, uri: &Url) {
        let mut tasks = self.analysis_tasks.lock().await;
        if let Some((_, task)) = tasks.get_mut(uri) {
            task.abort();
            let _ = task.await;
        }
        tasks.remove(uri);
    }

    async fn cancel_all_diagnostics(&self) {
        self.compiler_reapers.close();
        self.compiler_semaphore.close();
        let mut tasks = self.analysis_tasks.lock().await;
        for (_, task) in tasks.values() {
            task.abort();
        }
        for (_, task) in tasks.values_mut() {
            let _ = task.await;
        }
        tasks.clear();
        drop(tasks);
        self.compiler_reapers.wait().await;
    }

    async fn run_diagnostics(&self, uri: Url, version: i32) {
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
        let Ok(permit) = self.analysis_semaphore.clone().acquire_owned().await else {
            span.record("outcome", "unavailable");
            return;
        };
        let analysis_span = span.clone();
        let mut diagnostics = tokio::task::spawn_blocking(move || {
            analysis_span.in_scope(|| {
                let _permit = permit;
                lexical_diagnostics(&lexical_document)
            })
        })
        .await
        .unwrap_or_default();

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
            let compiler_config = self
                .compiler_config
                .read()
                .map(|config| config.clone())
                .unwrap_or_default();
            let compiler = compiler_diagnostics(
                source_graph,
                compiler_config,
                self.compiler_semaphore.clone(),
                self.compiler_results.clone(),
                self.compiler_reapers.clone(),
            )
            .await;
            if !diagnostics
                .iter()
                .any(|item| item.code == Some(NumberOrString::String("holes".into())))
            {
                let current_revision = self.document(&uri).map(|current| current.revision);
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
                self.replace_import_diagnostics(uri.clone(), version, imported)
                    .await;
            }
        }
        let current_revision = self.document(&uri).map(|current| current.revision);
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
    async fn replace_import_diagnostics(
        &self,
        root_uri: Url,
        version: i32,
        by_uri: HashMap<Url, Vec<Diagnostic>>,
    ) {
        let affected = {
            let Ok(mut snapshots) = self.imported_diagnostics.write() else {
                return;
            };
            let Some(affected) =
                replace_imported_diagnostics_if_current(&mut snapshots, root_uri, version, by_uri)
            else {
                return;
            };
            affected
        };
        self.publish_import_diagnostics(affected).await;
    }

    async fn remove_import_diagnostics(&self, root_uri: &Url) {
        let affected = {
            let Ok(mut snapshots) = self.imported_diagnostics.write() else {
                return;
            };
            let Some(previous) = snapshots.remove(root_uri) else {
                return;
            };
            previous.by_uri.into_keys().collect()
        };
        self.publish_import_diagnostics(affected).await;
    }

    #[tracing::instrument(name = "diagnostics.publish", skip_all, fields(kind = "imported", target_count = affected.len(), published_count = tracing::field::Empty))]
    async fn publish_import_diagnostics(&self, affected: HashSet<Url>) {
        let _publish = self.imported_diagnostic_publish.lock().await;
        let span = tracing::Span::current();
        let mut published_count = 0;
        for uri in affected {
            if self.document(&uri).is_some() {
                continue;
            }
            let diagnostics = self
                .imported_diagnostics
                .read()
                .map(|snapshots| {
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
                })
                .unwrap_or_default();
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
    fn watched_snapshot_context(&self, events: &[(Url, PathBuf)]) -> Option<HashMap<Url, i32>> {
        let database = self.workspace_db.read().ok()?;
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
        Some(affected)
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
                    PreparedDiskUpdate {
                        uri,
                        path,
                        snapshot,
                        imports,
                    }
                })
                .collect()
        })
        .await
    }

    #[tracing::instrument(name = "workspace.update", skip_all, fields(kind = "watched_files", event_count = events.len(), outcome = tracing::field::Empty))]
    async fn sync_watched_files(&self, events: Vec<(Url, PathBuf)>) -> HashMap<Url, i32> {
        if events.is_empty() {
            return HashMap::new();
        }
        let _serial = self.workspace_update_serial.lock().await;
        let Some(mut affected) = self.watched_snapshot_context(&events) else {
            return HashMap::new();
        };
        let Some(prepared) =
            Self::prepare_watched_updates(self.staging_semaphore.clone(), events).await
        else {
            return HashMap::new();
        };
        let updates = {
            let _workspace_update = self.workspace_updates.write().await;
            let Ok(mut database) = self.workspace_db.write() else {
                return HashMap::new();
            };
            let commit_span = tracing::info_span!(
                "workspace.commit",
                kind = "watched_files",
                file_count = prepared.len(),
                outcome = tracing::field::Empty,
            );
            let updates = commit_span.in_scope(|| {
                let mut updates = Vec::with_capacity(prepared.len());
                for update in prepared {
                    let Some((id, imports_changed)) = database.sync_disk_snapshot_prepared(
                        &update.path,
                        update.snapshot,
                        update.imports,
                    ) else {
                        continue;
                    };
                    if let Some(document) = database.open_document(&update.uri) {
                        affected.insert(document.uri.clone(), document.revision.0);
                    }
                    updates.push((id, imports_changed));
                }
                if !updates.is_empty() {
                    self.workspace_generation.fetch_add(1, Ordering::Release);
                }
                updates
            });
            commit_span.record("outcome", "committed");
            updates
        };
        for (id, imports_changed) in &updates {
            if *imports_changed {
                self.load_reachable_async(std::slice::from_ref(id)).await;
            }
        }
        let workspace_db = self.workspace_db.clone();
        let after = run_staging(self.staging_semaphore.clone(), move || {
            let Ok(database) = workspace_db.read() else {
                return HashMap::<Url, i32>::new();
            };
            let mut affected = HashMap::<Url, i32>::new();
            for (id, _) in updates {
                for document in database.dependents(id) {
                    affected.insert(document.uri.clone(), document.revision.0);
                }
            }
            affected
        })
        .await
        .unwrap_or_default();
        affected.extend(after);
        tracing::Span::current().record("outcome", "committed");
        affected
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    #[tracing::instrument(
        name = "lsp.request",
        skip_all,
        fields(method = "initialize", file_id = tracing::field::Empty, revision = tracing::field::Empty)
    )]
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let mut roots: Vec<PathBuf> = params
            .workspace_folders
            .unwrap_or_default()
            .into_iter()
            .filter_map(|folder| folder.uri.to_file_path().ok())
            .map(|path| normalize_path(&path))
            .collect();
        if roots.is_empty()
            && let Some(uri) = params.root_uri
            && let Ok(path) = uri.to_file_path()
        {
            roots.push(normalize_path(&path));
        }
        if let Ok(mut workspace_roots) = self.workspace_roots.write() {
            *workspace_roots = roots;
        }
        let supports_watch_registration = params
            .capabilities
            .workspace
            .as_ref()
            .and_then(|workspace| workspace.did_change_watched_files.as_ref())
            .and_then(|watchers| watchers.dynamic_registration)
            .unwrap_or(false);
        let supports_type_hierarchy_registration = params
            .capabilities
            .text_document
            .as_ref()
            .and_then(|text_document| text_document.type_hierarchy.as_ref())
            .and_then(|hierarchy| hierarchy.dynamic_registration)
            .unwrap_or(false);
        if let Ok(mut registration) = self.type_hierarchy_registration.write() {
            *registration = supports_type_hierarchy_registration;
        }
        if let Ok(mut watch_registration) = self.watch_registration.write() {
            *watch_registration = supports_watch_registration;
        }
        Ok(InitializeResult {
            capabilities: server_capabilities(),
            server_info: Some(ServerInfo {
                name: "bend2-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }
    #[tracing::instrument(name = "lsp.notification", skip_all, fields(method = "initialized"))]
    async fn initialized(&self, _: InitializedParams) {
        let mut registrations = Vec::new();
        if self.watch_registration.read().is_ok_and(|enabled| *enabled) {
            registrations.push(Registration {
                id: "bend2-lsp-bend-files".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(serde_json::json!({
                    "watchers":[{"globPattern":"**/*.bend","kind":7}]
                })),
            });
        }
        if self
            .type_hierarchy_registration
            .read()
            .is_ok_and(|enabled| *enabled)
        {
            registrations.push(Registration {
                id: "bend2-lsp-type-hierarchy".into(),
                method: "textDocument/prepareTypeHierarchy".into(),
                register_options: Some(serde_json::json!({
                    "documentSelector":[
                        {"language":"bend"},
                        {"language":"bend2"}
                    ]
                })),
            });
        }
        if !registrations.is_empty() {
            let _ = self.client.register_capability(registrations).await;
        }
    }

    #[tracing::instrument(
        name = "lsp.notification",
        skip_all,
        fields(method = "did_change_workspace_folders")
    )]
    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        if let Ok(mut roots) = self.workspace_roots.write() {
            for folder in params.event.removed {
                if let Ok(path) = folder.uri.to_file_path() {
                    let path = normalize_path(&path);
                    roots.retain(|root| *root != path);
                }
            }
            for folder in params.event.added {
                if let Ok(path) = folder.uri.to_file_path() {
                    let path = normalize_path(&path);
                    if !roots.contains(&path) {
                        roots.push(path);
                    }
                }
            }
        }
        for document in self
            .workspace_documents()
            .into_iter()
            .filter(|document| document.uri.scheme() == "untitled")
        {
            let path = self.document_path(&document.uri);
            self.open_workspace_document(document, path).await;
        }
        for document in self.workspace_documents() {
            let revision = document.revision.0;
            self.schedule_diagnostics(document.uri, revision).await;
        }
    }

    #[tracing::instrument(
        name = "lsp.notification",
        skip_all,
        fields(method = "did_change_configuration")
    )]
    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let (config, warnings) = CompilerConfig::from_settings(&params.settings);
        if let Ok(mut current) = self.compiler_config.write() {
            *current = config;
        }
        {
            let _load = self.base_module_load.lock().await;
            if let Ok(mut module) = self.base_module.write() {
                *module = None;
            }
            if let Ok(mut attempted) = self.base_module_attempted.write() {
                *attempted = false;
            }
        }
        for warning in warnings {
            self.client.log_message(MessageType::WARNING, warning).await;
        }
        let documents = self.workspace_documents();
        if documents.iter().any(|document| {
            analysis::imports(document)
                .iter()
                .any(|import| import.path_text(&document.text) == "Base")
        }) {
            self.load_prelude_module().await;
        }
        for document in documents {
            let revision = document.revision.0;
            self.schedule_diagnostics(document.uri, revision).await;
        }
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "shutdown"))]
    async fn shutdown(&self) -> Result<()> {
        self.cancel_all_diagnostics().await;
        Ok(())
    }
    #[tracing::instrument(name = "document.update", skip_all, fields(method = "did_open", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let item = params.text_document;
        let uri = item.uri;
        let version = item.version;
        let path = self.document_path(&uri);
        let Some(needs_prelude) = self
            .open_workspace_text(uri.clone(), item.language_id, version, item.text, path)
            .await
        else {
            return;
        };
        if needs_prelude {
            self.load_prelude_module().await;
        }
        let dependents = self.dependent_documents(&uri).await;
        self.schedule_diagnostics(uri, version).await;
        for document in dependents {
            let revision = document.revision.0;
            self.schedule_diagnostics(document.uri, revision).await;
        }
    }
    #[tracing::instrument(name = "document.update", skip_all, fields(method = "did_change", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        if !self.change_workspace_document(params).await {
            return;
        }
        if self.document(&uri).is_some_and(|document| {
            analysis::imports(&document)
                .iter()
                .any(|import| import.path_text(&document.text) == "Base")
        }) {
            self.load_prelude_module().await;
        }
        let dependents = self.dependent_documents(&uri).await;
        self.schedule_diagnostics(uri, version).await;
        for document in dependents {
            let revision = document.revision.0;
            self.schedule_diagnostics(document.uri, revision).await;
        }
    }
    #[tracing::instrument(
        name = "workspace.update",
        skip_all,
        fields(method = "did_change_watched_files")
    )]
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let events = params
            .changes
            .into_iter()
            .filter_map(|event| {
                let path = event.uri.to_file_path().ok()?;
                Some((event.uri, normalize_path(&path)))
            })
            .collect();
        for (uri, version) in self.sync_watched_files(events).await {
            self.schedule_diagnostics(uri, version).await;
        }
    }

    #[tracing::instrument(name = "document.update", skip_all, fields(method = "did_close", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.cancel_diagnostics(&uri).await;
        self.close_workspace_document(uri.clone()).await;
        self.remove_import_diagnostics(&uri).await;
        let publish_span = tracing::info_span!(
            "diagnostics.publish",
            kind = "clear",
            diagnostic_count = 0,
            outcome = tracing::field::Empty,
        );
        self.client
            .publish_diagnostics(uri.clone(), Vec::new(), None)
            .instrument(publish_span.clone())
            .await;
        publish_span.record("outcome", "published");
        for document in self.dependent_documents(&uri).await {
            let revision = document.revision.0;
            self.schedule_diagnostics(document.uri, revision).await;
        }
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "range_formatting", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let start = super::adapters::offset_at(&doc, Position::new(params.range.start.line, 0));
        let end = super::adapters::offset_at(&doc, params.range.end);
        if start > end {
            return Ok(Some(Vec::new()));
        }
        let selected = &doc.text[start..end];
        let formatted = format_bend(
            selected,
            params.options.tab_size as usize,
            params.options.insert_spaces,
        );
        if formatted == selected {
            return Ok(Some(Vec::new()));
        }
        let first_line = selected
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        let indent_length = first_line
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        let base_indent = &first_line[..indent_length];
        let mut new_text = String::with_capacity(formatted.len() + base_indent.len());
        for line in formatted.split_inclusive('\n') {
            if !line.trim_matches(['\r', '\n']).is_empty() {
                new_text.push_str(base_indent);
            }
            new_text.push_str(line);
        }
        Ok(Some(vec![TextEdit {
            range: Range::new(
                super::adapters::position_at(&doc, start),
                super::adapters::position_at(&doc, end),
            ),
            new_text,
        }]))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "on_type_formatting", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        let _workspace_read = self
            .document_read(&params.text_document_position.text_document.uri)
            .await;
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        if params.ch != "\n" {
            return Ok(Some(Vec::new()));
        }
        let offset = super::adapters::offset_at(&doc, td.position);
        let line_start = doc.text[..offset].rfind('\n').map_or(0, |index| index + 1);
        let previous = doc.text[..line_start]
            .trim_end_matches(['\r', '\n'])
            .rsplit('\n')
            .next()
            .unwrap_or("");
        let previous_indent = previous
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        let previous_indent_text = &previous[..previous_indent];
        let Some(opens_body) = super::formatter::opens_indented_body(&previous[previous_indent..])
        else {
            return Ok(Some(Vec::new()));
        };
        let unit = if params.options.insert_spaces {
            " ".repeat(params.options.tab_size as usize)
        } else {
            "\t".to_owned()
        };
        let desired = if opens_body {
            format!("{previous_indent_text}{unit}")
        } else {
            previous_indent_text.to_owned()
        };
        let line_end = doc.text[offset..]
            .find('\n')
            .map_or(doc.text.len(), |index| offset + index);
        let current_line = &doc.text[line_start..line_end];
        let current_indent = current_line
            .bytes()
            .take_while(|byte| matches!(byte, b' ' | b'\t'))
            .count();
        if current_line[..current_indent] == desired {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(vec![TextEdit {
            range: Range::new(
                super::adapters::position_at(&doc, line_start),
                super::adapters::position_at(&doc, line_start + current_indent),
            ),
            new_text: desired,
        }]))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "formatting", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let formatted = format_bend(
            &doc.text,
            params.options.tab_size as usize,
            params.options.insert_spaces,
        );
        if formatted == doc.text {
            return Ok(Some(Vec::new()));
        }
        Ok(Some(vec![TextEdit {
            range: Range {
                start: Position::new(0, 0),
                end: super::adapters::position_at(&doc, doc.text.len()),
            },
            new_text: formatted,
        }]))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "completion", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let _workspace_read = self
            .document_read(&params.text_document_position.text_document.uri)
            .await;
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = super::adapters::offset_at(&doc, td.position);
        if cursor_in_comment_or_string(&doc, offset) {
            return Ok(Some(CompletionResponse::Array(Vec::new())));
        }
        let syntax = &doc.syntax;
        let prefix_token = syntax
            .token_at_or_before(offset)
            .and_then(|id| syntax.token(id))
            .filter(|token| {
                matches!(
                    token.kind,
                    analysis::TokenKind::Identifier | analysis::TokenKind::Number
                ) && token.range.start <= offset
                    && offset <= token.range.end
            });
        let prefix_start = prefix_token.map_or(offset, |token| token.range.start);
        let prefix = &doc.text[prefix_start..offset];
        let alias = prefix_start
            .checked_sub(1)
            .and_then(|position| syntax.token_at_or_before(position))
            .and_then(|dot_id| {
                let dot = syntax.token(dot_id)?;
                if dot.kind != analysis::TokenKind::Punctuation
                    || syntax.token_text(&doc.text, dot_id) != Some(".")
                    || dot.range.end != prefix_start
                {
                    return None;
                }
                let alias_id = analysis::TokenId(dot_id.0.checked_sub(1)?);
                let alias = syntax.token(alias_id)?;
                (alias.kind == analysis::TokenKind::Identifier
                    && alias.range.end == dot.range.start)
                    .then(|| &doc.text[alias.range.start..alias.range.end])
            });
        if let Some(alias) = alias {
            if let Some((_, source)) = self.module_document(&doc, alias) {
                return Ok(Some(CompletionResponse::Array(
                    super::adapters::completion_items(analysis::module_completion_items(
                        &source, prefix,
                    )),
                )));
            }
            if let Some(module) = self.prelude_module(&doc) {
                return Ok(Some(CompletionResponse::Array(
                    super::adapters::completion_items(analysis::qualified_completion_items(
                        &module, alias, prefix,
                    )),
                )));
            }
        }
        let mut items = super::adapters::completion_items(analysis::completion_items(&doc, prefix));
        if !prefix.is_empty() {
            let mut labels: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();
            if let Some(module) = self.prelude_module(&doc) {
                items.extend(
                    super::adapters::completion_items(analysis::completion_items(&module, prefix))
                        .into_iter()
                        .filter(|item| labels.insert(item.label.clone())),
                );
            }
            for import in analysis::imports(&doc) {
                if let Some(alias) = import
                    .alias_text(&doc.text)
                    .filter(|alias| alias.starts_with(prefix))
                    && labels.insert(alias.to_owned())
                {
                    let mut item = CompletionItem::new_simple(
                        alias.to_owned(),
                        format!("Imported module {}", import.path_text(&doc.text)),
                    );
                    item.kind = Some(CompletionItemKind::MODULE);
                    items.push(item);
                }
            }
        }
        Ok(Some(CompletionResponse::Array(items)))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "signature_help", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let _workspace_read = self
            .document_read(&params.text_document_position_params.text_document.uri)
            .await;
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(
            analysis::signature_help(&doc, super::adapters::offset_at(&doc, td.position))
                .map(super::adapters::signature_help),
        )
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "prepare_call_hierarchy", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        let _workspace_read = self
            .document_read(&params.text_document_position_params.text_document.uri)
            .await;
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = super::adapters::offset_at(&doc, td.position);
        if cursor_in_comment_or_string(&doc, offset) {
            return Ok(None);
        }
        let selected = doc.syntax.token_at_or_before(offset).filter(|token| {
            doc.syntax.token(*token).is_some_and(|token| {
                token.kind == analysis::TokenKind::Identifier
                    && token.range.start <= offset
                    && offset <= token.range.end
            })
        });
        let resolved = selected
            .and_then(|token| doc.syntax.symbol_for_token(token))
            .filter(|symbol| {
                doc.syntax
                    .symbol_by_id(*symbol)
                    .is_some_and(|symbol| symbol.kind == analysis::SymbolKind::Function)
            })
            .map(|symbol| (doc.uri.clone(), doc.snapshot.clone(), symbol))
            .or_else(|| {
                let token = selected?;
                self.resolve_call_target(&doc, doc.syntax.call_for_token(token)?)
            });
        let Some((uri, snapshot, symbol)) = resolved else {
            return Ok(None);
        };
        let Some(symbol) = super::adapters::document_symbol_by_id(&snapshot, symbol) else {
            return Ok(None);
        };
        Ok(Some(vec![call_hierarchy_item(uri, symbol)]))
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "incoming_calls", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let _workspace_read = self.workspace_ready_read().await;
        let target = params.item;
        let mut calls = Vec::<(CallHierarchyItem, Vec<Range>)>::new();
        for caller_doc in self.hierarchy_documents() {
            if !Self::supported(&caller_doc) {
                continue;
            }
            for call in caller_doc.syntax.calls() {
                let Some(caller_id) = call.caller else {
                    continue;
                };
                let Some((target_uri, target_snapshot, target_id)) =
                    self.resolve_call_target(&caller_doc, call)
                else {
                    continue;
                };
                if target_uri != target.uri
                    || target_snapshot
                        .syntax
                        .symbol_by_id(target_id)
                        .is_none_or(|symbol| {
                            target_snapshot
                                .syntax
                                .name_text(&target_snapshot.text, symbol.name)
                                != target.name
                        })
                {
                    continue;
                }
                let Some(caller_symbol) =
                    super::adapters::document_symbol_by_id(&caller_doc, caller_id)
                else {
                    continue;
                };
                let range = super::adapters::range(&caller_doc, call.callee_range);
                if let Some((_, ranges)) = calls
                    .iter_mut()
                    .find(|(item, _)| item.uri == caller_doc.uri && item.name == caller_symbol.name)
                {
                    ranges.push(range);
                } else {
                    calls.push((
                        call_hierarchy_item(caller_doc.uri.clone(), caller_symbol),
                        vec![range],
                    ));
                }
            }
        }
        Ok(Some(
            calls
                .into_iter()
                .map(|(from, from_ranges)| CallHierarchyIncomingCall { from, from_ranges })
                .collect(),
        ))
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "outgoing_calls", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let _workspace_read = self.workspace_ready_read().await;
        let caller_item = params.item;
        let Some(caller_doc) = self.hierarchy_document(&caller_item.uri) else {
            return Ok(Some(Vec::new()));
        };
        self.ensure_prelude_module(&caller_doc).await;
        let Some(caller_id) = caller_doc
            .syntax
            .name_id(&caller_doc.text, &caller_item.name)
            .and_then(|name| caller_doc.syntax.symbol_by_name(name))
            .filter(|symbol| symbol.kind == analysis::SymbolKind::Function)
            .map(|symbol| symbol.id)
        else {
            return Ok(Some(Vec::new()));
        };
        let mut calls = Vec::<(CallHierarchyItem, Vec<Range>)>::new();
        for call in caller_doc.syntax.calls_from(caller_id) {
            let Some((target_uri, target_snapshot, target_id)) =
                self.resolve_call_target(&caller_doc, call)
            else {
                continue;
            };
            let Some(target) = super::adapters::document_symbol_by_id(&target_snapshot, target_id)
            else {
                continue;
            };
            if let Some((_, ranges)) = calls
                .iter_mut()
                .find(|(item, _)| item.uri == target_uri && item.name == target.name)
            {
                ranges.push(super::adapters::range(&caller_doc, call.callee_range));
            } else {
                calls.push((
                    call_hierarchy_item(target_uri, target),
                    vec![super::adapters::range(&caller_doc, call.callee_range)],
                ));
            }
        }
        Ok(Some(
            calls
                .into_iter()
                .map(|(to, from_ranges)| CallHierarchyOutgoingCall { to, from_ranges })
                .collect(),
        ))
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "prepare_type_hierarchy", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let _workspace_read = self
            .document_read(&params.text_document_position_params.text_document.uri)
            .await;
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = super::adapters::offset_at(&doc, td.position);
        let Some(token) = declaration_token_at(&doc, offset) else {
            return Ok(None);
        };
        let Some((uri, source, name)) = self.hierarchy_source(&doc, &token) else {
            return Ok(None);
        };
        let Some((symbol, _)) = type_hierarchy_symbol(&source, &name) else {
            return Ok(None);
        };
        Ok(Some(vec![type_hierarchy_item(uri, symbol)]))
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "supertypes", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let _workspace_read = self.document_read(&params.item.uri).await;
        let item = params.item;
        let Some(doc) = self.hierarchy_document(&item.uri) else {
            return Ok(Some(Vec::new()));
        };
        let parent = super::adapters::document_symbols(&doc)
            .into_iter()
            .find(|symbol| {
                symbol.kind == SymbolKind::STRUCT
                    && symbol.children.as_ref().is_some_and(|children| {
                        children.iter().any(|child| child.name == item.name)
                    })
            });
        Ok(Some(
            parent
                .map(|symbol| vec![type_hierarchy_item(doc.uri, symbol)])
                .unwrap_or_default(),
        ))
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "subtypes", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let _workspace_read = self.document_read(&params.item.uri).await;
        let item = params.item;
        let Some(doc) = self.hierarchy_document(&item.uri) else {
            return Ok(Some(Vec::new()));
        };
        let children = super::adapters::document_symbols(&doc)
            .into_iter()
            .find(|symbol| symbol.kind == SymbolKind::STRUCT && symbol.name == item.name)
            .and_then(|symbol| symbol.children)
            .unwrap_or_default()
            .into_iter()
            .map(|symbol| type_hierarchy_item(doc.uri.clone(), symbol))
            .collect();
        Ok(Some(children))
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "code_lens", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let mut lenses = Vec::new();
        for symbol in doc
            .syntax
            .symbols()
            .iter()
            .filter(|symbol| symbol.kind == analysis::SymbolKind::Function)
        {
            let detail = &doc.text[symbol.detail_range.start..symbol.detail_range.end];
            if detail.starts_with("law ") {
                continue;
            }
            let locations: Vec<Location> = doc
                .syntax
                .references(symbol.id)
                .filter(|reference| reference.kind != analysis::ReferenceKind::Declaration)
                .map(|reference| Location {
                    uri: doc.uri.clone(),
                    range: super::adapters::range(&doc, reference.range),
                })
                .collect();
            if locations.is_empty() {
                continue;
            }
            let count = locations.len();
            let selection_range = super::adapters::range(&doc, symbol.name_range);
            let selection_start = selection_range.start;
            lenses.push(CodeLens {
                range: selection_range,
                command: Some(tower_lsp::lsp_types::Command {
                    title: format!("{count} reference{}", if count == 1 { "" } else { "s" }),
                    command: "editor.action.showReferences".into(),
                    arguments: Some(vec![
                        serde_json::json!(doc.uri.as_str()),
                        serde_json::json!(selection_start),
                        serde_json::json!(locations),
                    ]),
                }),
                data: None,
            });
        }
        Ok(Some(lenses))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "inlay_hint", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let range = super::adapters::text_range(&doc, params.range);
        let hints = analysis::inlay_hints(&doc, range);
        Ok(Some(super::adapters::inlay_hints(&doc, hints)))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "code_action", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let uri = params.text_document.uri;
        let Some(doc) = self.document(&uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        if params
            .context
            .only
            .as_ref()
            .is_some_and(|only| !only.iter().any(|kind| kind == &CodeActionKind::QUICKFIX))
        {
            return Ok(Some(Vec::new()));
        }
        let requested_range = params.range;
        let mut actions = Vec::new();
        for diagnostic in params.context.diagnostics {
            if (diagnostic.range.end.line, diagnostic.range.end.character)
                < (requested_range.start.line, requested_range.start.character)
                || (requested_range.end.line, requested_range.end.character)
                    < (
                        diagnostic.range.start.line,
                        diagnostic.range.start.character,
                    )
            {
                continue;
            }
            if diagnostic.code != Some(NumberOrString::String("parsing".into())) {
                continue;
            }
            let Some(open) = diagnostic
                .message
                .strip_prefix("Unclosed '")
                .and_then(|message| message.chars().next())
            else {
                continue;
            };
            let Some(close) = (match open {
                '(' => Some(')'),
                '[' => Some(']'),
                '{' => Some('}'),
                _ => None,
            }) else {
                continue;
            };
            let insertion = super::adapters::position_at(
                &doc,
                code_end_offset(
                    &doc,
                    super::adapters::offset_at(&doc, diagnostic.range.start),
                ),
            );
            let edit = WorkspaceEdit {
                changes: Some(HashMap::from([(
                    uri.clone(),
                    vec![TextEdit {
                        range: Range::new(insertion, insertion),
                        new_text: close.to_string(),
                    }],
                )])),
                ..Default::default()
            };
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Insert '{close}'"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic]),
                edit: Some(edit),
                is_preferred: Some(true),
                ..Default::default()
            }));
        }
        Ok(Some(actions))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "folding_range", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(super::adapters::folding_ranges(
            analysis::folding_ranges(&doc),
        )))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "selection_range", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(
            params
                .positions
                .into_iter()
                .map(|position| {
                    let offset = super::adapters::offset_at(&doc, position);
                    let selection = analysis::selection_range(&doc, offset);
                    super::adapters::selection_range(&doc, selection)
                })
                .collect(),
        ))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "document_link", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let mut links = Vec::new();
        for import in analysis::imports(&doc) {
            let path = import.path_text(&doc.text);
            if !path.ends_with(".bend") {
                continue;
            }
            let target = self
                .workspace_db
                .read()
                .ok()
                .and_then(|database| database.import_target(&doc.uri, import.path));
            if let Some(target) = target {
                links.push(DocumentLink {
                    range: super::adapters::range(&doc, import.path),
                    target: Some(target.uri),
                    tooltip: Some(format!("Open {path}")),
                    data: None,
                });
            }
        }
        Ok(Some(links))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "semantic_tokens_full", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data: super::adapters::semantic_tokens(&doc, analysis::semantic_tokens(&doc)),
        })))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "document_symbol", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        Ok(Some(DocumentSymbolResponse::Nested(
            super::adapters::document_symbols(&doc),
        )))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "textDocument/hover", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let _workspace_read = self
            .document_read(&params.text_document_position_params.text_document.uri)
            .await;
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = super::adapters::offset_at(&doc, td.position);
        if let Some(symbol) = binding_at(&doc, offset) {
            let value = doc.syntax.binding_by_id(symbol).and_then(|binding| {
                let ty = doc.syntax.binding_type_range(&doc.text, symbol)?;
                let name = doc.syntax.name_text(&doc.text, binding.name);
                let ty = &doc.text[ty.start..ty.end];
                Some(format!("```bend\n{name}: {ty}\n```"))
            });
            return Ok(value.map(|value| Hover {
                contents: HoverContents::Markup(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value,
                }),
                range: None,
            }));
        }
        let Some(token) = declaration_token_at(&doc, super::adapters::offset_at(&doc, td.position))
        else {
            return Ok(None);
        };
        let imported = token.split_once('.').and_then(|(alias, member)| {
            self.module_document(&doc, alias)
                .and_then(|(_, source)| analysis::declaration_hover(&source, member))
        });
        let prelude = self
            .prelude_declaration(&doc, &token)
            .and_then(|module| analysis::declaration_hover(&module, &token));
        let Some(value) = analysis::declaration_hover(&doc, &token)
            .or(imported)
            .or(prelude)
            .or_else(|| static_hover(&token))
        else {
            return Ok(None);
        };
        Ok(Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        }))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "goto_definition", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let _workspace_read = self
            .document_read(&params.text_document_position_params.text_document.uri)
            .await;
        let td = params.text_document_position_params;
        let Some(doc) = tracing::info_span!("navigation.document_lookup")
            .in_scope(|| self.document(&td.text_document.uri))
        else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc)
            .instrument(tracing::info_span!("navigation.ensure_prelude"))
            .await;
        let (offset, in_comment_or_string) = tracing::info_span!("navigation.cursor_lookup")
            .in_scope(|| {
                let offset = super::adapters::offset_at(&doc, td.position);
                (offset, cursor_in_comment_or_string(&doc, offset))
            });
        if in_comment_or_string {
            return Ok(None);
        }
        let import = tracing::info_span!("navigation.import_lookup").in_scope(|| {
            analysis::imports(&doc).iter().find(|import| {
                let contains =
                    |range: analysis::TextRange| range.start <= offset && offset <= range.end;
                contains(import.path) || import.alias.is_some_and(contains)
            })
        });
        if let Some(import) = import {
            let path = import.path_text(&doc.text);
            let target_uri = tracing::info_span!("navigation.module_lookup").in_scope(|| {
                if path == "Base" {
                    self.prelude_module(&doc).map(|module| module.uri)
                } else {
                    self.workspace_db
                        .read()
                        .ok()
                        .and_then(|database| database.import_target(&doc.uri, import.path))
                        .map(|document| document.uri)
                }
            });
            if let Some(uri) = target_uri {
                let response = tracing::info_span!("navigation.location", scope = "import")
                    .in_scope(|| {
                        GotoDefinitionResponse::Scalar(Location {
                            uri,
                            range: Range::default(),
                        })
                    });
                return Ok(Some(response));
            }
            return Ok(None);
        }
        let module_uri = doc.syntax.token_at_or_before(offset).and_then(|id| {
            let cursor_token = doc.syntax.token(id)?;
            if cursor_token.kind != analysis::TokenKind::Identifier
                || !cursor_token.range.contains(offset)
                || doc.syntax.symbol_for_token(id).is_some()
                || id.0.checked_sub(1).is_some_and(|previous| {
                    doc.syntax
                        .token_text(&doc.text, analysis::TokenId(previous))
                        == Some(".")
                })
            {
                return None;
            }
            let alias = doc.syntax.token_text(&doc.text, id)?;
            self.module_document(&doc, alias).map(|(uri, _)| uri)
        });
        if let Some(uri) = module_uri {
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range: Range::default(),
            })));
        }
        let Some(token) = tracing::info_span!("navigation.token_lookup")
            .in_scope(|| declaration_token_at(&doc, offset))
        else {
            return Ok(None);
        };
        let local_range = tracing::info_span!("navigation.declaration_lookup", scope = "local")
            .in_scope(|| analysis::declaration_range(&doc, &token));
        if let Some(range) = local_range {
            let response =
                tracing::info_span!("navigation.location", scope = "local").in_scope(|| {
                    let range = super::adapters::range(&doc, range);
                    GotoDefinitionResponse::Scalar(Location {
                        uri: doc.uri,
                        range,
                    })
                });
            return Ok(Some(response));
        }
        let prelude = tracing::info_span!("navigation.declaration_lookup", scope = "prelude")
            .in_scope(|| {
                let module = self.prelude_declaration(&doc, &token)?;
                let range = analysis::declaration_range(&module, &token)?;
                Some((module, range))
            });
        if let Some((module, range)) = prelude {
            let response =
                tracing::info_span!("navigation.location", scope = "prelude").in_scope(|| {
                    let range = super::adapters::range(&module, range);
                    GotoDefinitionResponse::Scalar(Location {
                        uri: module.uri,
                        range,
                    })
                });
            return Ok(Some(response));
        }
        let Some((alias, name)) = token.split_once('.') else {
            return Ok(None);
        };
        let target = tracing::info_span!("navigation.module_lookup")
            .in_scope(|| self.module_document(&doc, alias));
        let Some((target_uri, target_text)) = target else {
            return Ok(None);
        };
        let range = tracing::info_span!("navigation.declaration_lookup", scope = "imported")
            .in_scope(|| analysis::declaration_range(&target_text, name));
        let Some(range) = range else {
            return Ok(None);
        };
        let response =
            tracing::info_span!("navigation.location", scope = "imported").in_scope(|| {
                GotoDefinitionResponse::Scalar(Location {
                    uri: target_uri,
                    range: super::adapters::range(&target_text, range),
                })
            });
        Ok(Some(response))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "workspace_symbol", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let _workspace_read = self.workspace_ready_read().await;
        let documents = self.indexed_documents();
        let prelude_document = documents
            .iter()
            .find(|document| {
                analysis::imports(document)
                    .iter()
                    .any(|import| import.path_text(&document.text) == "Base")
            })
            .cloned();
        let includes_base = prelude_document.is_some();
        if let Some(document) = prelude_document {
            self.ensure_prelude_module(&document).await;
        }
        let mut symbols = Vec::new();
        for doc in documents {
            if Self::supported(&doc) {
                symbols.extend(super::adapters::workspace_symbols(
                    &doc,
                    &doc.uri,
                    &params.query,
                ));
            }
        }
        if includes_base
            && let Some(module) = self
                .base_module
                .read()
                .ok()
                .and_then(|module| module.clone())
        {
            symbols.extend(super::adapters::workspace_symbols(
                &module,
                &module.uri,
                &params.query,
            ));
        }
        Ok(Some(symbols))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "references", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let _workspace_read = self.workspace_ready_read().await;
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = super::adapters::offset_at(&doc, td.position);
        if let Some(symbol) = binding_at(&doc, offset) {
            return Ok(Some(
                binding_ranges(&doc, symbol, params.context.include_declaration)
                    .into_iter()
                    .map(|range| Location {
                        uri: doc.uri.clone(),
                        range: super::adapters::range(&doc, range),
                    })
                    .collect(),
            ));
        }
        let Some(token) = declaration_token_at(&doc, offset) else {
            return Ok(None);
        };
        let (target_uri, target_text, name) = self.symbol_source(&doc, &token);
        if analysis::declaration_range(&target_text, &name).is_none() {
            return Ok(None);
        }
        Ok(Some(self.symbol_references(
            &target_uri,
            &name,
            params.context.include_declaration,
        )))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "document_highlight", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let _workspace_read = self
            .document_read(&params.text_document_position_params.text_document.uri)
            .await;
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = super::adapters::offset_at(&doc, td.position);
        if let Some(symbol) = binding_at(&doc, offset) {
            return Ok(Some(
                binding_ranges(&doc, symbol, true)
                    .into_iter()
                    .map(|range| DocumentHighlight {
                        range: super::adapters::range(&doc, range),
                        kind: Some(DocumentHighlightKind::TEXT),
                    })
                    .collect(),
            ));
        }
        let Some(token) = declaration_token_at(&doc, offset) else {
            return Ok(None);
        };
        let (target_uri, target_text, name) = self.symbol_source(&doc, &token);
        if analysis::declaration_range(&target_text, &name).is_none() {
            return Ok(None);
        }
        Ok(Some(
            self.symbol_references(&target_uri, &name, true)
                .into_iter()
                .filter(|location| location.uri == doc.uri)
                .map(|location| DocumentHighlight {
                    range: location.range,
                    kind: Some(DocumentHighlightKind::TEXT),
                })
                .collect(),
        ))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "rename", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let _workspace_read = self.workspace_ready_read().await;
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = super::adapters::offset_at(&doc, td.position);
        if let Some(symbol) = binding_at(&doc, offset) {
            let edits = binding_ranges(&doc, symbol, true)
                .into_iter()
                .map(|range| TextEdit {
                    range: super::adapters::range(&doc, range),
                    new_text: params.new_name.clone(),
                })
                .collect();
            return Ok(Some(WorkspaceEdit {
                changes: Some(HashMap::from([(doc.uri.clone(), edits)])),
                ..Default::default()
            }));
        }
        let Some(token) = declaration_token_at(&doc, offset) else {
            return Ok(None);
        };
        let (target_uri, target_text, name) = self.symbol_source(&doc, &token);
        if analysis::declaration_range(&target_text, &name).is_none() {
            return Ok(None);
        }
        let mut changes = HashMap::<Url, Vec<TextEdit>>::new();
        for location in self.symbol_references(&target_uri, &name, true) {
            changes.entry(location.uri).or_default().push(TextEdit {
                range: location.range,
                new_text: params.new_name.clone(),
            });
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "goto_type_definition", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let _workspace_read = self
            .document_read(&params.text_document_position_params.text_document.uri)
            .await;
        let td = params.text_document_position_params;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        self.ensure_prelude_module(&doc).await;
        let offset = super::adapters::offset_at(&doc, td.position);
        if let Some(symbol) = binding_at(&doc, offset) {
            let range = doc
                .syntax
                .binding_type_range(&doc.text, symbol)
                .and_then(|ty| {
                    let expression = &doc.text[ty.start..ty.end];
                    doc.syntax
                        .symbols()
                        .iter()
                        .filter(|symbol| symbol.kind == analysis::SymbolKind::Struct)
                        .find(|symbol| {
                            expression
                                .split(|character: char| {
                                    !character.is_ascii_alphanumeric() && character != '_'
                                })
                                .any(|name| name == doc.syntax.name_text(&doc.text, symbol.name))
                        })
                        .map(|symbol| symbol.name_range)
                });
            return Ok(range.map(|range| {
                GotoDefinitionResponse::Scalar(Location {
                    uri: doc.uri.clone(),
                    range: super::adapters::range(&doc, range),
                })
            }));
        }
        let Some(token) = declaration_token_at(&doc, super::adapters::offset_at(&doc, td.position))
        else {
            return Ok(None);
        };
        let imported = token.split_once('.').and_then(|(alias, member)| {
            self.module_document(&doc, alias)
                .map(|(uri, source)| (uri, source, member.to_owned()))
        });
        let local = analysis::type_declaration_range(&doc, &token)
            .map(|_| (doc.uri.clone(), doc.snapshot.clone(), token.clone()));
        let prelude = self
            .prelude_module(&doc)
            .filter(|module| analysis::type_declaration_range(module, &token).is_some())
            .map(|module| (module.uri, module.snapshot, token.clone()));
        let (target_uri, target_text, name) = imported
            .or(local)
            .or(prelude)
            .unwrap_or_else(|| (doc.uri.clone(), doc.snapshot.clone(), token.clone()));
        let range = analysis::type_declaration_range(&target_text, &name);
        let Some(range) = range else {
            return Ok(None);
        };
        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range: super::adapters::range(&target_text, range),
        })))
    }
}

// Declaration queries must not reinterpret a lexical binding or a module alias
// under the cursor as the declaration named by the entire qualified expression.
fn declaration_token_at(snapshot: &DocumentSnapshot, offset: usize) -> Option<String> {
    if cursor_in_comment_or_string(snapshot, offset) {
        return None;
    }
    let syntax = &snapshot.syntax;
    let cursor = syntax.token_at_or_before(offset)?;
    let token = syntax.token(cursor)?;
    if offset < token.range.start || offset > token.range.end {
        return None;
    }
    let mut root = cursor;
    while let Some(previous) = root.0.checked_sub(2) {
        let qualifier = analysis::TokenId(previous);
        let dot = analysis::TokenId(previous + 1);
        let Some(qualifier_token) = syntax.token(qualifier) else {
            break;
        };
        let Some(dot_token) = syntax.token(dot) else {
            break;
        };
        let Some(member_token) = syntax.token(root) else {
            break;
        };
        if qualifier_token.kind != analysis::TokenKind::Identifier
            || syntax.token_text(&snapshot.text, dot) != Some(".")
            || qualifier_token.range.end != dot_token.range.start
            || dot_token.range.end != member_token.range.start
        {
            break;
        }
        root = qualifier;
    }
    if syntax
        .symbol_for_token(root)
        .is_some_and(|symbol| syntax.binding_by_id(symbol).is_some())
    {
        return None;
    }
    let root_text = syntax.token_text(&snapshot.text, root)?;
    if root == cursor
        && analysis::imports(snapshot).iter().any(|import| {
            import.alias_text(&snapshot.text) == Some(root_text)
                || import.path.contains(offset)
                || import.alias.is_some_and(|range| range.contains(offset))
        })
    {
        return None;
    }
    Some(token_at(snapshot, offset))
}

fn binding_at(snapshot: &DocumentSnapshot, offset: usize) -> Option<analysis::SymbolId> {
    let syntax = &snapshot.syntax;
    let token_id = syntax.token_at_or_before(offset)?;
    let token = syntax.token(token_id)?;
    if token.kind != analysis::TokenKind::Identifier
        || offset < token.range.start
        || offset > token.range.end
    {
        return None;
    }
    let symbol = syntax.symbol_for_token(token_id)?;
    syntax.binding_by_id(symbol).map(|_| symbol)
}

fn binding_ranges(
    snapshot: &DocumentSnapshot,
    symbol: analysis::SymbolId,
    include_declaration: bool,
) -> Vec<analysis::TextRange> {
    let declaration = snapshot.syntax.symbol_declaration_range(symbol);
    snapshot
        .syntax
        .references(symbol)
        .map(|reference| reference.range)
        .filter(|range| include_declaration || Some(*range) != declaration)
        .collect()
}
#[cfg(test)]
mod workspace_readiness_tests {
    use super::{DocumentRevisionSync, Revision, wait_for_captured_revision};
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn captured_revision_completes_after_commit() -> Result<(), Box<dyn std::error::Error>> {
        let sync = Arc::new(DocumentRevisionSync::new(None));
        let ticket = sync
            .reserve(Revision(1))
            .ok_or_else(|| std::io::Error::other("reserve revision"))?;
        let wait = wait_for_captured_revision(sync.status.subscribe(), ticket.generation);
        assert!(ticket.mark_committed());
        tokio::time::timeout(Duration::from_secs(1), wait).await?;
        Ok(())
    }

    #[tokio::test]
    async fn captured_revision_does_not_wait_for_a_newer_reservation()
    -> Result<(), Box<dyn std::error::Error>> {
        let sync = Arc::new(DocumentRevisionSync::new(None));
        let old = sync
            .reserve(Revision(1))
            .ok_or_else(|| std::io::Error::other("reserve old revision"))?;
        let wait = wait_for_captured_revision(sync.status.subscribe(), old.generation);
        let _new = sync
            .reserve(Revision(2))
            .ok_or_else(|| std::io::Error::other("reserve newer revision"))?;
        tokio::time::timeout(Duration::from_secs(1), wait).await?;
        Ok(())
    }

    #[tokio::test]
    async fn captured_revision_completes_after_failed_stage_or_close()
    -> Result<(), Box<dyn std::error::Error>> {
        let failed = Arc::new(DocumentRevisionSync::new(None));
        let ticket = failed
            .reserve(Revision(1))
            .ok_or_else(|| std::io::Error::other("reserve revision"))?;
        let wait = wait_for_captured_revision(failed.status.subscribe(), ticket.generation);
        drop(ticket);
        tokio::time::timeout(Duration::from_secs(1), wait).await?;

        let closed = Arc::new(DocumentRevisionSync::new(None));
        let ticket = closed
            .reserve(Revision(1))
            .ok_or_else(|| std::io::Error::other("reserve revision"))?;
        let wait = wait_for_captured_revision(closed.status.subscribe(), ticket.generation);
        let mut status = *closed.status.borrow();
        status.desired = None;
        status.committed = None;
        closed.status.send_replace(status);
        tokio::time::timeout(Duration::from_secs(1), wait).await?;
        Ok(())
    }
}

#[cfg(test)]
mod staging_tests {
    use super::run_staging;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    use tokio::sync::Semaphore;

    #[tokio::test]
    async fn staging_blocking_work_respects_its_concurrency_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        const LIMIT: usize = 4;
        let semaphore = Arc::new(Semaphore::new(LIMIT));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let semaphore = semaphore.clone();
                let active = active.clone();
                let maximum = maximum.clone();
                tokio::spawn(async move {
                    run_staging(semaphore, move || {
                        let running = active.fetch_add(1, Ordering::Relaxed) + 1;
                        maximum.fetch_max(running, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(10));
                        active.fetch_sub(1, Ordering::Relaxed);
                    })
                    .await
                    .ok_or_else(|| std::io::Error::other("staging task should complete"))?;
                    Ok::<(), std::io::Error>(())
                })
            })
            .collect();

        for task in tasks {
            task.await??;
        }
        assert_eq!(maximum.load(Ordering::Relaxed), LIMIT);
        Ok(())
    }
}
#[cfg(test)]
mod diagnostics_publication_tests {
    use super::{
        DocumentRevisionSync, Revision, diagnostics_revision_is_current, publish_if_current,
        replace_imported_diagnostics_if_current,
    };
    use std::{
        collections::{HashMap, HashSet},
        sync::Arc,
    };
    use url::Url;

    #[tokio::test]
    async fn held_root_v1_result_cannot_publish_after_v2_commits()
    -> Result<(), Box<dyn std::error::Error>> {
        let sync = Arc::new(DocumentRevisionSync::new(None));
        let held_v1 = (Revision(1), "v1 diagnostics".to_owned());
        let v2_ticket = sync
            .reserve(Revision(2))
            .ok_or_else(|| std::io::Error::other("reserve v2"))?;
        assert!(v2_ticket.mark_committed());
        let committed = sync.status.borrow().committed;
        assert!(diagnostics_revision_is_current(committed, Revision(2)));

        let mut published = Vec::new();
        assert!(
            publish_if_current(
                committed,
                Revision(2),
                "v2 diagnostics".to_owned(),
                |result| {
                    published.push((Revision(2), result));
                    std::future::ready(())
                }
            )
            .await
        );
        assert!(
            !publish_if_current(committed, held_v1.0, held_v1.1, |result| {
                published.push((Revision(1), result));
                std::future::ready(())
            })
            .await
        );
        assert_eq!(published, vec![(Revision(2), "v2 diagnostics".to_owned())]);
        Ok(())
    }

    #[test]
    fn held_import_v1_result_cannot_replace_v2_snapshot() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = Url::parse("file:///workspace/root.bend")?;
        let v2_import = Url::parse("file:///workspace/f099.bend")?;
        let held_v1_import = Url::parse("file:///workspace/f098.bend")?;
        let mut snapshots = HashMap::new();

        let affected = replace_imported_diagnostics_if_current(
            &mut snapshots,
            root.clone(),
            2,
            HashMap::from([(v2_import.clone(), Vec::new())]),
        )
        .ok_or_else(|| std::io::Error::other("v2 import should replace the snapshot"))?;
        assert_eq!(affected, HashSet::from([v2_import.clone()]));

        let stale_affected = replace_imported_diagnostics_if_current(
            &mut snapshots,
            root.clone(),
            1,
            HashMap::from([(held_v1_import.clone(), Vec::new())]),
        );
        assert_eq!(stale_affected, None);
        let current = snapshots
            .get(&root)
            .ok_or_else(|| std::io::Error::other("retain v2 snapshot"))?;
        assert_eq!(current.version, 2);
        assert_eq!(
            current.by_uri.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([v2_import])
        );
        Ok(())
    }
}
