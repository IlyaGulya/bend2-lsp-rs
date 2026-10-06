use super::super::{adapters, lsp::Backend};
use super::shared::{binding_at, binding_ranges, declaration_token_at};
use crate::analysis;
use std::collections::HashMap;
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        CodeLens, CodeLensParams, DocumentHighlight, DocumentHighlightKind,
        DocumentHighlightParams, Location, ReferenceParams, RenameParams, TextEdit, WorkspaceEdit,
    },
};
use url::Url;

impl Backend {
    pub(in crate::server) async fn handle_code_lens(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
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
                    range: adapters::range(&doc, reference.range),
                })
                .collect();
            if locations.is_empty() {
                continue;
            }
            let count = locations.len();
            let selection_range = adapters::range(&doc, symbol.name_range);
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

    pub(in crate::server) async fn handle_references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let _workspace_read = self.workspace_ready_read().await;
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = adapters::offset_at(&doc, td.position);
        if let Some(symbol) = binding_at(&doc, offset) {
            return Ok(Some(
                binding_ranges(&doc, symbol, params.context.include_declaration)
                    .into_iter()
                    .map(|range| Location {
                        uri: doc.uri.clone(),
                        range: adapters::range(&doc, range),
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

    pub(in crate::server) async fn handle_document_highlight(
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
        let offset = adapters::offset_at(&doc, td.position);
        if let Some(symbol) = binding_at(&doc, offset) {
            return Ok(Some(
                binding_ranges(&doc, symbol, true)
                    .into_iter()
                    .map(|range| DocumentHighlight {
                        range: adapters::range(&doc, range),
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

    pub(in crate::server) async fn handle_rename(
        &self,
        params: RenameParams,
    ) -> Result<Option<WorkspaceEdit>> {
        let _workspace_read = self.workspace_ready_read().await;
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = adapters::offset_at(&doc, td.position);
        if let Some(symbol) = binding_at(&doc, offset) {
            let edits = binding_ranges(&doc, symbol, true)
                .into_iter()
                .map(|range| TextEdit {
                    range: adapters::range(&doc, range),
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
}
