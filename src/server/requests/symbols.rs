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
        let documents = self.indexed_documents();
        let includes_base = documents.iter().any(|document| {
            analysis::imports(document)
                .iter()
                .any(|import| import.path_text(&document.text) == "Base")
        });
        let query = analysis::WorkspaceSymbolQuery::new(&params.query);
        let capacity = if query.is_empty() {
            documents
                .iter()
                .filter(|document| Self::supported(document))
                .map(|document| document.syntax.symbols().len())
                .sum()
        } else {
            0
        };
        let mut symbols = Vec::with_capacity(capacity);
        for doc in documents {
            if Self::supported(&doc) {
                symbols.extend(adapters::workspace_symbols(&doc, &doc.uri, &query));
            }
        }
        if includes_base && let Some(module) = self.compiler.base_module.read().clone() {
            if query.is_empty() {
                symbols.reserve(module.syntax.symbols().len());
            }
            symbols.extend(adapters::workspace_symbols(&module, &module.uri, &query));
        }
        Ok(Some(symbols))
    }
}
