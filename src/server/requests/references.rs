use super::super::{adapters, lsp::Backend};
use super::import_edits::versioned_edit;
use crate::{
    analysis,
    workspace::{Document, SemanticView},
};
use std::collections::HashMap;
use tower_lsp::{
    jsonrpc::{Error, Result},
    lsp_types::{
        CodeLens, CodeLensParams, DocumentHighlight, DocumentHighlightKind,
        DocumentHighlightParams, Location, PrepareRenameResponse, ReferenceParams, RenameParams,
        TextDocumentPositionParams, TextEdit, WorkspaceEdit,
    },
};
use url::Url;

fn alias_at(
    document: &Document,
    view: &SemanticView,
    offset: usize,
) -> Option<analysis::IndexedImport> {
    let token = document.syntax.token_at_or_before(offset)?;
    let range = document.syntax.token(token)?.range;
    if offset < range.start || offset > range.end {
        return None;
    }
    let file = view.file_id(&document.uri)?;
    analysis::imports(document).iter().copied().find(|import| {
        if !import.valid_syntax {
            return false;
        }
        let Some(alias) = import.alias else {
            return false;
        };
        import.alias == Some(range)
            || import
                .alias_text(&document.text)
                .and_then(|name| view.alias_rename_ranges(file, alias, name))
                .is_some_and(|ranges| ranges.contains(&range))
    })
}

impl Backend {
    fn rename_alias(
        &self,
        document: &Document,
        view: &SemanticView,
        import: analysis::IndexedImport,
        new_name: &str,
    ) -> Result<WorkspaceEdit> {
        let alias = import
            .alias
            .ok_or_else(|| Error::invalid_params("missing module alias"))?;
        let file = view
            .file_id(&document.uri)
            .ok_or_else(Error::content_modified)?;
        let ranges = view
            .alias_rename_ranges(file, alias, new_name)
            .ok_or_else(|| {
                Error::invalid_params(
                    "Module alias rename would capture or conflict with a complete name",
                )
            })?;
        let database = self.workspace.read();
        for context in database.views_for_file(file) {
            let other_file = view
                .physical_uri(file)
                .and_then(|uri| context.physical_file_id(uri))
                .ok_or_else(Error::content_modified)?;
            if context
                .alias_rename_ranges(other_file, alias, new_name)
                .as_ref()
                != Some(&ranges)
            {
                return Err(Error::invalid_params(
                    "Module alias rename disagrees across entry contexts",
                ));
            }
        }
        let edits = ranges
            .into_iter()
            .map(|range| TextEdit {
                range: adapters::range(document, range),
                new_text: new_name.to_owned(),
            })
            .collect();
        Ok(versioned_edit(document, edits))
    }

    fn reject_import_path_rename(document: &Document, offset: usize) -> Result<()> {
        if analysis::imports(document)
            .iter()
            .any(|import| import.path.start <= offset && offset <= import.path.end)
        {
            return Err(Error::invalid_params(
                "Import paths cannot be renamed with Rename Symbol; rename the file or folder in the project tree instead",
            ));
        }
        Ok(())
    }

    pub(in crate::server) async fn handle_prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let _read = self
            .semantic_reference_read(&params.text_document.uri, params.position)
            .await;
        let Some(document) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&document) {
            return Ok(None);
        }
        let offset = adapters::offset_at(&document, params.position);
        Self::reject_import_path_rename(&document, offset)?;
        if document.syntax.is_in_comment_or_string(offset) {
            return Ok(None);
        }
        let Some(view) = self.semantic_view(&document.uri) else {
            return Ok(None);
        };
        if let Some(import) = alias_at(&document, &view, offset) {
            let Some(range) = import.alias else {
                return Ok(None);
            };
            let placeholder = document.text[range.start..range.end].to_owned();
            if self
                .rename_alias(&document, &view, import, &placeholder)
                .is_err()
            {
                return Ok(None);
            }
            let token_range = document
                .syntax
                .token_at_or_before(offset)
                .and_then(|token| document.syntax.token(token))
                .map_or(range, |token| token.range);
            return Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
                range: adapters::range(&document, token_range),
                placeholder,
            }));
        }
        let Some(selection) = self.semantic_selection(&document, offset) else {
            return Ok(None);
        };
        if selection
            .view
            .target_site(selection.occurrence.target)
            .is_none()
        {
            return Ok(None);
        }
        let range = selection.occurrence.site.range;
        let placeholder = document.text[range.start..range.end].to_owned();
        if !self.semantic_rename_is_safe(&selection, &placeholder) {
            return Ok(None);
        }
        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: adapters::range(&document, range),
            placeholder,
        }))
    }
    pub(in crate::server) async fn handle_code_lens(
        &self,
        params: CodeLensParams,
    ) -> Result<Option<Vec<CodeLens>>> {
        let _workspace_read = self
            .semantic_reference_document_read(&params.text_document.uri)
            .await;
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
            if analysis::declaration_metadata(&doc, symbol.id)
                .is_some_and(|site| site.kind == analysis::DeclarationKind::Law)
            {
                continue;
            }
            let Some(selection) =
                self.semantic_selection(&doc, symbol.name_range.end.saturating_sub(1))
            else {
                continue;
            };
            let locations = self.semantic_references(&selection, false);
            if locations.is_empty() {
                continue;
            }
            let count = locations.len();
            let selection_range = adapters::range(&doc, selection.occurrence.site.range);
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
        let td = params.text_document_position;
        let _workspace_read = self
            .semantic_reference_read(&td.text_document.uri, td.position)
            .await;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = adapters::offset_at(&doc, td.position);
        let Some(selection) = self.semantic_selection(&doc, offset) else {
            return Ok(None);
        };
        if selection
            .view
            .target_site(selection.occurrence.target)
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(self.semantic_references(
            &selection,
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
        let Some(selection) = self.semantic_selection(&doc, offset) else {
            return Ok(None);
        };
        if selection
            .view
            .target_site(selection.occurrence.target)
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(
            self.semantic_references(&selection, true)
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
        let td = params.text_document_position;
        let _workspace_read = self
            .semantic_reference_read(&td.text_document.uri, td.position)
            .await;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = adapters::offset_at(&doc, td.position);
        Self::reject_import_path_rename(&doc, offset)?;
        if let Some(view) = self.semantic_view(&doc.uri)
            && let Some(import) = alias_at(&doc, &view, offset)
        {
            return self
                .rename_alias(&doc, &view, import, &params.new_name)
                .map(Some);
        }
        let Some(selection) = self.semantic_selection(&doc, offset) else {
            return Ok(None);
        };
        if selection
            .view
            .target_site(selection.occurrence.target)
            .is_none()
        {
            return Ok(None);
        }
        if !self.semantic_rename_is_safe(&selection, &params.new_name) {
            return Err(Error::invalid_params(
                "new name is invalid or rename would capture a name or conflict across entry contexts",
            ));
        }
        let mut changes = HashMap::<Url, Vec<TextEdit>>::new();
        for location in self.semantic_references(&selection, true) {
            changes.entry(location.uri).or_default().push(TextEdit {
                range: location.range,
                new_text: params.new_name.clone(),
            });
        }
        Ok(Some(self.semantic_workspace_edit(changes)))
    }
}
