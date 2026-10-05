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
        let mut symbols = Vec::new();
        for doc in documents {
            if Self::supported(&doc) {
                symbols.extend(adapters::workspace_symbols(&doc, &doc.uri, &params.query));
            }
        }
        if includes_base && let Some(module) = self.compiler.base_module.read().clone() {
            symbols.extend(adapters::workspace_symbols(
                &module,
                &module.uri,
                &params.query,
            ));
        }
        Ok(Some(symbols))
    }
}
