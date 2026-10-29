use super::super::{adapters, lsp::Backend};
use crate::analysis;
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        DocumentSymbolParams, DocumentSymbolResponse, SymbolInformation, WorkspaceSymbolParams,
    },
};

impl Backend {
    pub(in crate::server) async fn handle_document_symbol(
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
            adapters::document_symbols(&doc),
        )))
    }

    pub(in crate::server) async fn handle_symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let _workspace_read = self.workspace_read_with_prelude(None).await;
        let workspace = self.workspace.read();
        let includes_base = workspace.indexed_snapshots().any(|(_, _, snapshot)| {
            analysis::imports(snapshot).iter().any(|import| {
                import.valid_syntax
                    && import.alias.is_none()
                    && import.path_text(&snapshot.text) == "Base"
            })
        });
        let query = analysis::WorkspaceSymbolQuery::new(&params.query);
        let capacity = if query.is_empty() {
            workspace
                .indexed_snapshots()
                .filter(|(_, language_id, _)| matches!(*language_id, "bend" | "bend2"))
                .map(|(_, _, snapshot)| snapshot.syntax.symbols().len())
                .sum()
        } else {
            0
        };
        let mut symbols = Vec::with_capacity(capacity);
        for (uri, language_id, snapshot) in workspace.indexed_snapshots() {
            if matches!(language_id, "bend" | "bend2") {
                symbols.extend(adapters::workspace_symbols(snapshot, uri, &query));
            }
        }
        drop(workspace);
        if includes_base && let Some(module) = self.compiler.base_module.read().clone() {
            if query.is_empty() {
                symbols.reserve(module.syntax.symbols().len());
            }
            symbols.extend(adapters::workspace_symbols(&module, &module.uri, &query));
        }
        Ok(Some(symbols))
    }
}
