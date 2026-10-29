use super::capabilities::server_capabilities;
use super::compiler::CompilerConfig;
#[cfg(test)]
use super::diagnostics::{
    diagnostics_revision_is_current, publish_if_current, replace_imported_diagnostics_if_current,
};
use super::lint::LintChange;
#[cfg(test)]
use super::orchestration::run_staging;
#[cfg(test)]
use super::revision::{DocumentRevisionSync, wait_for_captured_revision};
use crate::analysis;
#[cfg(test)]
use crate::analysis::Revision;
use crate::workspace::normalize_path;
use std::{future::Future, path::PathBuf, sync::Arc};
use tower_lsp::{
    Client, LanguageServer,
    jsonrpc::Result,
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
        CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
        CodeActionParams, CodeActionResponse, CodeLens, CodeLensParams, CompletionParams,
        CompletionResponse, DidChangeConfigurationParams, DidChangeTextDocumentParams,
        DidChangeWatchedFilesParams, DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DidSaveTextDocumentParams, DocumentFormattingParams,
        DocumentHighlight, DocumentHighlightParams, DocumentLink, DocumentLinkParams,
        DocumentOnTypeFormattingParams, DocumentRangeFormattingParams, DocumentSymbolParams,
        DocumentSymbolResponse, ExecuteCommandParams, FoldingRange, FoldingRangeParams,
        GotoDefinitionParams, GotoDefinitionResponse, Hover, HoverParams, InitializeParams,
        InitializeResult, InitializedParams, InlayHint, InlayHintParams, Location, MessageType,
        PrepareRenameResponse, ReferenceParams, Registration, RenameFilesParams, RenameParams,
        SelectionRange, SelectionRangeParams, SemanticTokensParams, SemanticTokensResult,
        ServerInfo, SignatureHelp, SignatureHelpParams, SymbolInformation,
        TextDocumentPositionParams, TextEdit, TypeHierarchyItem, TypeHierarchyPrepareParams,
        TypeHierarchySubtypesParams, TypeHierarchySupertypesParams, WorkspaceEdit,
        WorkspaceSymbolParams,
    },
};
use tracing::Instrument;
use url::Url;

#[derive(Clone)]
pub(super) struct Backend {
    pub(super) client: Client,
    pub(super) workspace: Arc<super::workspace_service::WorkspaceService>,
    pub(super) compiler: Arc<super::compiler_service::CompilerService>,
    pub(super) diagnostics: Arc<super::diagnostics::DiagnosticsService>,
    pub(super) registration: Arc<super::state::State<super::workspace_service::RegistrationState>>,
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
        *self.workspace.roots.write() = roots;
        self.workspace.start_discovery().await;
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
        {
            let mut registration = self.registration.write();
            registration.type_hierarchy = supports_type_hierarchy_registration;
            registration.watch = supports_watch_registration;
            registration.lint.disabled_code_actions = params
                .capabilities
                .text_document
                .as_ref()
                .and_then(|document| document.code_action.as_ref())
                .and_then(|actions| actions.disabled_support)
                .unwrap_or(false);
            registration.lint.work_done_progress = params
                .capabilities
                .window
                .as_ref()
                .and_then(|window| window.work_done_progress)
                .unwrap_or(false);
        }
        let mut result = InitializeResult {
            capabilities: server_capabilities(),
            ..InitializeResult::default()
        };
        result.server_info = Some(ServerInfo {
            name: "bend2-lsp".into(),
            version: Some(env!("CARGO_PKG_VERSION").into()),
        });
        Ok(result)
    }
    #[tracing::instrument(name = "lsp.notification", skip_all, fields(method = "initialized"))]
    async fn initialized(&self, _: InitializedParams) {
        let mut registrations = Vec::new();
        if self.registration.read().watch {
            registrations.push(Registration {
                id: "bend2-lsp-bend-files".into(),
                method: "workspace/didChangeWatchedFiles".into(),
                register_options: Some(serde_json::json!({
                    "watchers":[
                        {"globPattern":"**/*.bend","kind":7},
                        {"globPattern":"**/bend-lint.json","kind":7},
                        {"globPattern":"**/bend-lint-rules/**","kind":7}
                    ]
                })),
            });
        }
        if self.registration.read().type_hierarchy {
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
        let compatibility = self.check_compiler_compatibility().await;
        if let Some(warning) = compatibility.warning() {
            self.client.log_message(MessageType::WARNING, warning).await;
        }
    }

    #[tracing::instrument(
        name = "lsp.notification",
        skip_all,
        fields(method = "did_change_workspace_folders")
    )]
    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        let operation = async {
            self.diagnostics.invalidate_context();
            let _operation = self.workspace.file_operations.write().await;
            self.workspace.cancel_discovery().await;
            let _serial = self.workspace.update_serial.lock().await;
            let _update = self.workspace.updates.write().await;
            {
                let mut roots = self.workspace.roots.write();
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
            let roots = self.workspace.roots.read().clone();
            self.workspace.commit(None, None, |database| {
                database.retain_discovered_roots(&roots);
                Some(())
            });
            drop(_update);
            drop(_serial);
            for document in self
                .workspace_documents()
                .into_iter()
                .filter(|document| document.uri.scheme() == "untitled")
            {
                let path = self.document_path(&document.uri);
                self.open_workspace_document(document, path).await;
            }
            self.workspace.start_discovery().await;
            for document in self.workspace_documents() {
                let revision = document.revision.0;
                self.schedule_diagnostics(document.uri.as_ref().clone(), revision).await;
            }
            true
        };
        self.apply_lint_change(LintChange::Workspace, operation)
            .await;
    }

    #[tracing::instrument(
        name = "lsp.notification",
        skip_all,
        fields(method = "did_change_configuration")
    )]
    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let change = LintChange::settings(&params.settings);
        let operation = async {
            self.diagnostics.invalidate_context();
            let _operation = self.workspace.document_operation().await;
            let (config, warnings) = CompilerConfig::from_settings(&params.settings);
            {
                let _load = self.compiler.base_module_load.lock().await;
                let _workspace_update = self.workspace.updates.write().await;
                self.workspace.commit(None, None, |_database| {
                    *self.compiler.config.write() = config;
                    *self.compiler.base_module.write() = None;
                    *self.compiler.base_module_attempted.write() = false;
                    Some(())
                });
            }
            self.compiler.invalidate_compatibility().await;
            let compatibility = self.check_compiler_compatibility().await;
            if let Some(warning) = compatibility.warning() {
                self.client.log_message(MessageType::WARNING, warning).await;
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
                self.schedule_diagnostics(document.uri.as_ref().clone(), revision).await;
            }
            true
        };
        self.apply_lint_change(change, operation).await;
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "shutdown"))]
    async fn shutdown(&self) -> Result<()> {
        self.cancel_all_diagnostics().await;
        Ok(())
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "execute_command"))]
    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        self.handle_lint_command(params).await
    }
    #[tracing::instrument(name = "document.update", skip_all, fields(method = "did_open", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let item = params.text_document;
        let uri = item.uri;
        let version = item.version;
        let enabled = self.lint_scheduling_enabled();
        let change = enabled.then(|| LintChange::Open {
            uri: uri.clone(),
            revision: version,
        });
        let operation = async {
            let _operation = self.workspace.document_operation().await;
            let path = self.document_path(&uri);
            let Some(needs_prelude) = self
                .open_workspace_text(uri.clone(), item.language_id, version, item.text, path)
                .await
            else {
                return (false, None);
            };
            // Lint can be enabled while this open is still staged. Sampling
            // right after the commit keeps the steady disabled path free of an
            // identity clone and of any lint plan; enablement that arrives
            // later is enumerated by Settings, which now sees this committed
            // document.
            let deferred = (!enabled && self.lint_scheduling_enabled()).then(|| LintChange::Open {
                uri: uri.clone(),
                revision: version,
            });
            if needs_prelude {
                self.load_prelude_module().await;
            }
            let dependents = self.dependent_documents(&uri).await;
            self.schedule_diagnostics(uri, version).await;
            for document in dependents {
                let revision = document.revision.0;
                self.schedule_diagnostics(document.uri.as_ref().clone(), revision).await;
            }
            (true, deferred)
        };
        self.apply_document_lint_change(change, operation).await;
    }
    #[tracing::instrument(name = "document.update", skip_all, fields(method = "did_change", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;
        let enabled = self.lint_scheduling_enabled();
        let change = enabled.then(|| LintChange::Change {
            uri: uri.clone(),
            revision: version,
        });
        let operation = async {
            let _operation = self.workspace.document_operation().await;
            if !self.change_workspace_document(params).await {
                return (false, None);
            }
            // Same reconciliation as didOpen: a rejected or superseded change
            // returns before this sample, so it never fences newer work.
            let deferred =
                (!enabled && self.lint_scheduling_enabled()).then(|| LintChange::Change {
                    uri: uri.clone(),
                    revision: version,
                });
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
                self.schedule_diagnostics(document.uri.as_ref().clone(), revision).await;
            }
            (true, deferred)
        };
        self.apply_document_lint_change(change, operation).await;
    }
    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        if !self.lint_scheduling_enabled() {
            // A disabled analyzer must not let saves overtake pending opens.
            let _read = self.document_read(&uri).await;
            return;
        }
        let change = LintChange::Save { uri: uri.clone() };
        self.apply_lint_change(change, async {
            let _read = self.document_read(&uri).await;
            self.document(&uri).is_some()
        })
        .await;
    }

    #[tracing::instrument(
        name = "workspace.update",
        skip_all,
        fields(method = "did_change_watched_files")
    )]
    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        let events: Vec<_> = params
            .changes
            .into_iter()
            .filter_map(|event| {
                let path = event.uri.to_file_path().ok()?;
                Some((event.uri, normalize_path(&path)))
            })
            .collect();
        let change = LintChange::Watched(events.iter().map(|(_, path)| path.clone()).collect());
        let operation = async {
            let _operation = self.workspace.document_operation().await;
            for (uri, version) in self.sync_watched_files(events).await {
                self.schedule_diagnostics(uri, version).await;
            }
            true
        };
        self.apply_lint_change(change, operation).await;
    }

    #[tracing::instrument(name = "document.update", skip_all, fields(method = "did_close", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let change = LintChange::Close {
            uri: params.text_document.uri.clone(),
        };
        let operation = async {
            let _operation = self.workspace.document_operation().await;
            let uri = params.text_document.uri;
            let Some(closed) = self.workspace.begin_close(&uri) else {
                return false;
            };
            // Queue close before later workspace queries without delaying diagnostic
            // cleanup behind an unrelated staged disk read.
            let (_serial, ()) = tokio::join!(self.workspace.update_serial.lock(), async {
                self.detach_import_diagnostics(&uri, closed).await;
                self.cancel_diagnostics(&uri, closed).await;
            });
            if !self.close_workspace_document(uri.clone(), closed).await {
                return false;
            }
            // Keep close cleanup before any reopened document can commit. A stale
            // epoch must not clear newer diagnostics or imported contributions.
            let _workspace_view = self.workspace.updates.read().await;
            if !self.workspace.is_closed(closed) {
                return false;
            }
            self.remove_import_diagnostics(&uri).await;
            let publish_span = tracing::info_span!(
                "diagnostics.publish",
                kind = "clear",
                diagnostic_count = 0,
                outcome = tracing::field::Empty,
            );
            self.diagnostics.documents.write().remove(&uri);
            self.publish_coordinated_diagnostics(std::collections::HashSet::from([uri.clone()]))
                .instrument(publish_span.clone())
                .await;
            publish_span.record("outcome", "published");
            let dependents = self.dependent_documents(&uri).await;
            if !self.workspace.is_closed(closed) {
                return false;
            }
            for document in dependents {
                let revision = document.revision.0;
                self.schedule_diagnostics(document.uri.as_ref().clone(), revision).await;
            }
            true
        };
        self.apply_lint_change(change, operation).await;
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "range_formatting", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn range_formatting(
        &self,
        params: DocumentRangeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.handle_range_formatting(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "on_type_formatting", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn on_type_formatting(
        &self,
        params: DocumentOnTypeFormattingParams,
    ) -> Result<Option<Vec<TextEdit>>> {
        self.handle_on_type_formatting(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "formatting", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        self.handle_formatting(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "completion", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.handle_completion(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "signature_help", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        self.handle_signature_help(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "prepare_call_hierarchy", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        self.handle_prepare_call_hierarchy(params).await
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "incoming_calls", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        self.handle_incoming_calls(params).await
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "outgoing_calls", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        self.handle_outgoing_calls(params).await
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "prepare_type_hierarchy", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.handle_prepare_type_hierarchy(params).await
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "supertypes", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.handle_supertypes(params).await
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "subtypes", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.handle_subtypes(params).await
    }

    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "code_lens", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn code_lens(&self, params: CodeLensParams) -> Result<Option<Vec<CodeLens>>> {
        self.handle_code_lens(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "inlay_hint", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn inlay_hint(&self, params: InlayHintParams) -> Result<Option<Vec<InlayHint>>> {
        self.handle_inlay_hint(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "code_action", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        self.handle_code_action(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "folding_range", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        self.handle_folding_range(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "selection_range", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn selection_range(
        &self,
        params: SelectionRangeParams,
    ) -> Result<Option<Vec<SelectionRange>>> {
        self.handle_selection_range(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "document_link", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn document_link(&self, params: DocumentLinkParams) -> Result<Option<Vec<DocumentLink>>> {
        self.handle_document_link(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "semantic_tokens_full", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        self.handle_semantic_tokens_full(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "document_symbol", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.handle_document_symbol(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "textDocument/hover", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.handle_hover(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "goto_definition", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.handle_goto_definition(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "workspace_symbol", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        self.handle_symbol(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "references", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.handle_references(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "document_highlight", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        self.handle_document_highlight(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "prepare_rename", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.handle_prepare_rename(params).await
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "rename", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.handle_rename(params).await
    }
    #[tracing::instrument(
        name = "lsp.request",
        skip_all,
        fields(method = "workspace/willRenameFiles")
    )]
    async fn will_rename_files(&self, params: RenameFilesParams) -> Result<Option<WorkspaceEdit>> {
        self.handle_will_rename_files(params).await
    }
    #[tracing::instrument(
        name = "lsp.notification",
        skip_all,
        fields(method = "workspace/didRenameFiles")
    )]
    async fn did_rename_files(&self, params: RenameFilesParams) {
        let paths = params
            .files
            .iter()
            .filter_map(|file| {
                let old = Url::parse(file.old_uri.as_str())
                    .ok()?
                    .to_file_path()
                    .ok()?;
                let new = Url::parse(file.new_uri.as_str())
                    .ok()?
                    .to_file_path()
                    .ok()?;
                Some((normalize_path(&old), normalize_path(&new)))
            })
            .collect();
        let operation = async {
            let _operation = self.workspace.file_operations.write().await;
            self.handle_did_rename_files(params).await;
            true
        };
        self.apply_lint_change(LintChange::Rename(paths), operation)
            .await;
    }
    #[tracing::instrument(name = "lsp.request", skip_all, fields(method = "goto_type_definition", file_id = tracing::field::Empty, revision = tracing::field::Empty))]
    async fn goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.handle_goto_type_definition(params).await
    }
}

impl Backend {
    // Lint can be enabled while a document mutation is still pending, while the
    // default-disabled path runs that mutation without a lint plan. The
    // mutation samples enablement again after it commits so an applied document
    // is routed through the one coordinator lifecycle instead of waiting for a
    // save or edit that may never come. Lint that was already enabled is fenced
    // before the mutation; a rejected or superseded mutation never reaches the
    // sample, so this entry cannot fence newer work.
    async fn apply_document_lint_change<F>(&self, change: Option<LintChange>, operation: F) -> bool
    where
        F: Future<Output = (bool, Option<LintChange>)>,
    {
        let Some(change) = change else {
            let (applied, deferred) = operation.await;
            if applied && let Some(deferred) = deferred {
                self.apply_lint_change(deferred, std::future::ready(true))
                    .await;
            }
            return applied;
        };
        self.apply_lint_change(change, async move { operation.await.0 })
            .await
    }
}

#[cfg(test)]
mod workspace_readiness_tests {
    use super::{DocumentRevisionSync, Revision, wait_for_captured_revision};
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn captured_revision_completes_after_commit() -> Result<(), Box<dyn std::error::Error>> {
        let sync = Arc::new(DocumentRevisionSync::new(None));
        let ticket = sync
            .reserve(Revision(1))?
            .ok_or_else(|| std::io::Error::other("reserve revision"))?;
        let wait = wait_for_captured_revision(sync.subscribe(), ticket.generation());
        assert!(ticket.mark_committed()?);
        tokio::time::timeout(Duration::from_secs(1), wait).await??;
        Ok(())
    }

    #[tokio::test]
    async fn captured_revision_does_not_wait_for_a_newer_reservation()
    -> Result<(), Box<dyn std::error::Error>> {
        let sync = Arc::new(DocumentRevisionSync::new(None));
        let old = sync
            .reserve(Revision(1))?
            .ok_or_else(|| std::io::Error::other("reserve old revision"))?;
        let wait = wait_for_captured_revision(sync.subscribe(), old.generation());
        let _new = sync
            .reserve(Revision(2))?
            .ok_or_else(|| std::io::Error::other("reserve newer revision"))?;
        tokio::time::timeout(Duration::from_secs(1), wait).await??;
        Ok(())
    }

    #[tokio::test]
    async fn captured_revision_completes_after_failed_stage_or_close()
    -> Result<(), Box<dyn std::error::Error>> {
        let failed = Arc::new(DocumentRevisionSync::new(None));
        let ticket = failed
            .reserve(Revision(1))?
            .ok_or_else(|| std::io::Error::other("reserve revision"))?;
        let wait = wait_for_captured_revision(failed.subscribe(), ticket.generation());
        drop(ticket);
        tokio::time::timeout(Duration::from_secs(1), wait).await??;

        let closed = Arc::new(DocumentRevisionSync::new(None));
        let ticket = closed
            .reserve(Revision(1))?
            .ok_or_else(|| std::io::Error::other("reserve revision"))?;
        let wait = wait_for_captured_revision(closed.subscribe(), ticket.generation());
        closed.close()?;
        tokio::time::timeout(Duration::from_secs(1), wait).await??;
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
            .reserve(Revision(2))?
            .ok_or_else(|| std::io::Error::other("reserve v2"))?;
        assert!(v2_ticket.mark_committed()?);
        let committed = sync.status()?.committed();
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
        assert_eq!(
            current.by_uri.keys().cloned().collect::<HashSet<_>>(),
            HashSet::from([v2_import.clone()])
        );
        let cleared =
            replace_imported_diagnostics_if_current(&mut snapshots, root, 3, HashMap::new())
                .ok_or_else(|| std::io::Error::other("newer clean result should clear imports"))?;
        assert_eq!(cleared, HashSet::from([v2_import]));
        Ok(())
    }
}

#[cfg(test)]
mod notification_ordering_tests {
    use super::Backend;
    use crate::server::{
        compiler::CompilerReapers, diagnostics::DiagnosticsService,
        workspace_service::WorkspaceService,
    };
    use serde_json::{Value, json};
    use std::{
        error::Error,
        fs,
        future::{Future, poll_fn},
        io,
        pin::Pin,
        sync::Arc,
        task::Poll,
        time::Duration,
    };
    use tokio::sync::Semaphore;
    use tower_lsp::{
        LanguageServer, LspService,
        lsp_types::{Hover, HoverParams},
    };
    use url::Url;

    const OLD: &str = "def identity: U64\n  1\n";
    const NEW: &str = "def identity: U32\n  1\n";

    fn backend() -> Backend {
        let workspace = Arc::new(WorkspaceService {
            staging: Arc::new(Semaphore::new(1)),
            ..WorkspaceService::default()
        });
        let (service, _socket) = LspService::new(|client| {
            Backend::new(
                client,
                workspace,
                Arc::new(DiagnosticsService::default()),
                Arc::new(CompilerReapers::default()),
            )
        });
        service.inner().clone()
    }

    async fn first_poll<F: Future + ?Sized>(mut future: Pin<&mut F>) -> Poll<F::Output> {
        poll_fn(|context| Poll::Ready(future.as_mut().poll(context))).await
    }

    fn hover_params(uri: &Url) -> Result<HoverParams, serde_json::Error> {
        serde_json::from_value(json!({
            "textDocument":{"uri":uri},"position":{"line":0,"character":5}
        }))
    }

    fn assert_new_hover(hover: Option<Hover>) -> Result<(), Box<dyn Error>> {
        let hover: Value = serde_json::to_value(hover)?;
        let text = hover["contents"]["value"]
            .as_str()
            .ok_or_else(|| io::Error::other("hover must expose the newly notified document"))?;
        assert!(
            text.contains("def identity: U32") && !text.contains("U64"),
            "{hover}"
        );
        Ok(())
    }

    async fn seed(backend: &Backend, uri: &Url) -> Result<(), Box<dyn Error>> {
        backend
            .open_workspace_text(uri.clone(), "bend".into(), 1, OLD.into(), None)
            .await
            .ok_or_else(|| io::Error::other("seed initial document"))?;
        Ok(())
    }

    // Poll each actual public handler exactly once while a real coordinator
    // publication is blocked. No sleep or retry may hide a missing reservation.
    #[tokio::test]
    async fn lint_publication_contention_cannot_overtake_did_change() -> Result<(), Box<dyn Error>>
    {
        let backend = backend();
        let uri = Url::parse("untitled:lint-change.bend")?;
        seed(&backend, &uri).await?;
        let staging = backend.workspace.staging.acquire().await?;
        let publication = backend.diagnostics.imported_publish.lock().await;
        let mut change = backend.did_change(serde_json::from_value(json!({
            "textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":NEW}]
        }))?);
        assert!(first_poll(change.as_mut()).await.is_pending());
        let mut hover = backend.hover(hover_params(&uri)?);
        let first = first_poll(hover.as_mut()).await;
        assert!(first.is_pending(), "hover overtook didChange: {first:?}");
        drop(publication);
        drop(staging);
        let ((), hover) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(change, hover)
        })
        .await?;
        assert_new_hover(hover?)?;
        backend.cancel_all_diagnostics().await;
        Ok(())
    }

    #[tokio::test]
    async fn lint_publication_contention_cannot_overtake_did_open() -> Result<(), Box<dyn Error>> {
        let backend = backend();
        let uri = Url::parse("untitled:lint-open.bend")?;
        let staging = backend.workspace.staging.acquire().await?;
        let publication = backend.diagnostics.imported_publish.lock().await;
        let mut open = backend.did_open(serde_json::from_value(json!({
            "textDocument":{"uri":uri,"languageId":"bend","version":1,"text":NEW}
        }))?);
        assert!(first_poll(open.as_mut()).await.is_pending());
        let mut hover = backend.hover(hover_params(&uri)?);
        let first = first_poll(hover.as_mut()).await;
        assert!(first.is_pending(), "hover overtook didOpen: {first:?}");
        drop(publication);
        drop(staging);
        let ((), hover) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(open, hover) })
                .await?;
        assert_new_hover(hover?)?;
        backend.cancel_all_diagnostics().await;
        Ok(())
    }

    #[tokio::test]
    async fn did_save_waits_for_open_even_when_lint_cleanup_is_contended()
    -> Result<(), Box<dyn Error>> {
        let backend = backend();
        let uri = Url::parse("untitled:lint-open-save.bend")?;
        let staging = backend.workspace.staging.acquire().await?;
        let publication = backend.diagnostics.imported_publish.lock().await;
        let mut open = backend.did_open(serde_json::from_value(json!({
            "textDocument":{"uri":uri,"languageId":"bend","version":1,"text":NEW}
        }))?);
        assert!(first_poll(open.as_mut()).await.is_pending());
        let mut save = backend.did_save(serde_json::from_value(json!({
            "textDocument":{"uri":uri}
        }))?);
        let first = first_poll(save.as_mut()).await;
        assert!(
            first.is_pending(),
            "didSave skipped an unreserved didOpen: {first:?}"
        );
        drop(publication);
        drop(staging);
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(open, save) }).await?;
        assert_new_hover(backend.hover(hover_params(&uri)?).await?)?;
        backend.cancel_all_diagnostics().await;
        Ok(())
    }

    #[tokio::test]
    async fn lint_publication_contention_cannot_overtake_did_close() -> Result<(), Box<dyn Error>> {
        let backend = backend();
        let uri = Url::parse("untitled:lint-close.bend")?;
        seed(&backend, &uri).await?;
        let publication = backend.diagnostics.imported_publish.lock().await;
        let mut close = backend.did_close(serde_json::from_value(json!({
            "textDocument":{"uri":uri}
        }))?);
        assert!(first_poll(close.as_mut()).await.is_pending());
        let mut hover = backend.hover(hover_params(&uri)?);
        let first = first_poll(hover.as_mut()).await;
        if let Poll::Ready(result) = &first {
            assert!(
                result.as_ref().is_ok_and(Option::is_none),
                "hover retained a document after didClose: {first:?}"
            );
        }
        drop(publication);
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            match first {
                Poll::Ready(result) => {
                    close.await;
                    result
                }
                Poll::Pending => {
                    let ((), result) = tokio::join!(close, hover);
                    result
                }
            }
        })
        .await??;
        assert!(
            result.is_none(),
            "closed document must not remain hoverable"
        );
        backend.cancel_all_diagnostics().await;
        Ok(())
    }

    // Lint is disabled when didOpen arrives, so the mutation must run without a
    // lint plan. A concurrent settings notification then enables lint while the
    // document is still staged. Once the open applies, the coordinator has to
    // reconcile the committed document with the current enablement; otherwise
    // the inputs stay unscheduled until a save or edit that may never happen.
    #[tokio::test]
    async fn enabling_lint_during_a_pending_open_checks_the_committed_document()
    -> Result<(), Box<dyn Error>> {
        let backend = backend();
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("main.bend");
        fs::write(&path, OLD)?;
        fs::write(directory.path().join("bend-lint.json"), "{\"rules\":{}}")?;
        // An existing but unrunnable analyzer file satisfies the opt-in path
        // validation and fails preparation deterministically, so this regression
        // observes scheduling without executing project code.
        let analyzer = directory.path().join("bend-lint-stub");
        fs::write(&analyzer, b"")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&analyzer, fs::Permissions::from_mode(0o755))?;
        }
        let uri = Url::from_file_path(&path)
            .map_err(|()| io::Error::other("the document path is not a file URI"))?;

        let staging = backend.workspace.staging.acquire().await?;
        let mut open = backend.did_open(serde_json::from_value(json!({
            "textDocument": {
                "uri": uri,
                "languageId": "bend",
                "version": 1,
                "text": NEW
            }
        }))?);
        assert!(first_poll(open.as_mut()).await.is_pending());
        backend
            .did_change_configuration(serde_json::from_value(json!({
                "settings": {
                    "bend": {
                        "lint": {
                            "enabled": true,
                            "path": analyzer,
                            "arguments": [],
                            "dangerousCodeActions": false,
                            "timeoutMs": 10_000
                        }
                    }
                }
            }))?)
            .await;
        drop(staging);
        tokio::time::timeout(Duration::from_secs(5), open).await?;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = backend
                .execute_command(serde_json::from_value(json!({
                    "command": "bend2.lint.status",
                    "arguments": [{"uri": uri}]
                }))?)
                .await?
                .unwrap_or(Value::Null);
            let state = status["state"].as_str().unwrap_or_default();
            if matches!(state, "failed" | "skipped") {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::other(format!(
                    "enabling bend.lint while didOpen was still pending must check the committed \
                     document; lint status stayed {state:?}"
                ))
                .into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        backend.cancel_all_diagnostics().await;
        Ok(())
    }
}
