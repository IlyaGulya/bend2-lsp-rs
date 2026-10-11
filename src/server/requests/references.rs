use super::super::{adapters, lsp::Backend};
use super::shared::{binding_at, binding_ranges, declaration_token_at};
use crate::{analysis, workspace::Document};
use std::collections::HashMap;
use tower_lsp::{
    jsonrpc::{Error, Result},
    lsp_types::{
        CodeLens, CodeLensParams, DocumentChanges, DocumentHighlight, DocumentHighlightKind,
        DocumentHighlightParams, Location, OneOf, OptionalVersionedTextDocumentIdentifier,
        PrepareRenameResponse, ReferenceParams, RenameParams, TextDocumentEdit,
        TextDocumentPositionParams, TextEdit, WorkspaceEdit,
    },
};
use url::Url;

enum RenameKind {
    Alias(analysis::IndexedImport),
    Binding(analysis::SymbolId),
    Symbol { uri: Url, name: String },
}

struct RenameTarget {
    range: analysis::TextRange,
    kind: RenameKind,
}

fn alias_root(document: &Document, token: analysis::TokenId) -> bool {
    let syntax = &document.syntax;
    let Some(range) = syntax.token(token).map(|token| token.range) else {
        return false;
    };
    // Import paths are module links, never alias usages, even when a path
    // segment has the same spelling as an explicit alias.
    if analysis::imports(document)
        .iter()
        .any(|import| import.path.contains(range.start))
        || syntax.symbol_for_token(token).is_some()
        || token.0.checked_sub(1).is_some_and(|previous| {
            let previous = analysis::TokenId(previous);
            syntax
                .token(previous)
                .is_some_and(|token| token.range.end == range.start)
                && syntax.token_text(&document.text, previous) == Some(".")
        })
    {
        return false;
    }
    let dot = analysis::TokenId(token.0 + 1);
    syntax
        .token(dot)
        .is_some_and(|dot_token| dot_token.range.start == range.end)
        && syntax.token_text(&document.text, dot) == Some(".")
}

fn alias_at(document: &Document, offset: usize) -> Option<analysis::IndexedImport> {
    let token = document.syntax.token_at_or_before(offset)?;
    let range = document.syntax.token(token)?.range;
    if offset < range.start || offset > range.end {
        return None;
    }
    let text = document.syntax.token_text(&document.text, token)?;
    analysis::imports(document).iter().copied().find(|import| {
        import.alias == Some(range)
            || import.alias_text(&document.text) == Some(text) && alias_root(document, token)
    })
}

fn alias_ranges(document: &Document, import: analysis::IndexedImport) -> Vec<analysis::TextRange> {
    let Some(alias) = import.alias else {
        return Vec::new();
    };
    let mut ranges = vec![alias];
    if let Some(name) = document.syntax.name_id(
        &document.text,
        import.alias_text(&document.text).unwrap_or(""),
    ) {
        ranges.extend(
            document
                .syntax
                .qualified_references(name)
                .filter_map(|reference| reference.qualifier_token)
                .filter(|token| alias_root(document, *token))
                .filter_map(|token| document.syntax.token(token).map(|token| token.range)),
        );
    }
    ranges
}

fn rename_alias(
    document: &Document,
    import: analysis::IndexedImport,
    new_name: &str,
) -> Result<WorkspaceEdit> {
    let ranges = alias_ranges(document, import);
    let bytes = new_name.as_bytes();
    let valid = bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        && !analysis::SyntaxIndex::is_keyword_text(new_name);
    let conflict = analysis::imports(document).iter().any(|other| {
        other.alias != import.alias
            && (other.alias_text(&document.text) == Some(new_name)
                || other.alias_text(&document.text) == import.alias_text(&document.text))
    }) || document
        .syntax
        .name_id(&document.text, new_name)
        .is_some_and(|name| {
            document.syntax.symbol_by_name(name).is_some()
                || document.syntax.constructor_by_name(name).is_some()
                || ranges.iter().any(|range| {
                    document
                        .syntax
                        .bindings_at(range.start)
                        .any(|binding| binding.name == name)
                })
        });
    if !valid || conflict {
        return Err(Error::invalid_params(
            "Module alias rename would produce an invalid or conflicting name",
        ));
    }
    let edits = ranges
        .into_iter()
        .map(|range| {
            OneOf::Left(TextEdit {
                range: adapters::range(document, range),
                new_text: new_name.into(),
            })
        })
        .collect();
    Ok(WorkspaceEdit {
        document_changes: Some(DocumentChanges::Edits(vec![TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: document.uri.clone(),
                version: Some(document.revision.0),
            },
            edits,
        }])),
        ..Default::default()
    })
}

impl Backend {
    fn rename_target(&self, document: &Document, offset: usize) -> Result<Option<RenameTarget>> {
        if analysis::imports(document)
            .iter()
            .any(|import| import.path.start <= offset && offset <= import.path.end)
        {
            return Err(Error::invalid_params(
                "Import paths cannot be renamed with Rename Symbol; rename the file or folder in the project tree instead",
            ));
        }
        if document.syntax.is_in_comment_or_string(offset) {
            return Ok(None);
        }
        let Some(cursor) = document.syntax.token_at_or_before(offset) else {
            return Ok(None);
        };
        let Some(token) = document.syntax.token(cursor) else {
            return Ok(None);
        };
        if token.kind != analysis::TokenKind::Identifier
            || offset < token.range.start
            || offset > token.range.end
        {
            return Ok(None);
        }
        let range = document
            .syntax
            .reference_for_token(cursor)
            .map_or(token.range, |reference| reference.range);
        if let Some(import) = alias_at(document, offset) {
            return Ok(Some(RenameTarget {
                range,
                kind: RenameKind::Alias(import),
            }));
        }
        if let Some(symbol) = binding_at(document, offset) {
            return Ok(Some(RenameTarget {
                range,
                kind: RenameKind::Binding(symbol),
            }));
        }
        let Some(token) = declaration_token_at(document, offset) else {
            return Ok(None);
        };
        let (uri, snapshot, name) = self.symbol_source(document, &token);
        // The workspace reference index supports declarations represented as
        // symbols. Do not offer a preview for targets rename cannot edit.
        if snapshot
            .syntax
            .name_id(&snapshot.text, &name)
            .and_then(|name| snapshot.syntax.symbol_by_name(name))
            .is_none()
        {
            return Ok(None);
        }
        Ok(Some(RenameTarget {
            range,
            kind: RenameKind::Symbol { uri, name },
        }))
    }

    pub(in crate::server) async fn handle_prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let _workspace_read = self.workspace_ready_read().await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = adapters::offset_at(&doc, params.position);
        let Some(target) = self.rename_target(&doc, offset)? else {
            return Ok(None);
        };
        let placeholder = match target.kind {
            RenameKind::Symbol { name, .. } => name,
            RenameKind::Alias(_) | RenameKind::Binding(_) => {
                doc.text[target.range.start..target.range.end].to_owned()
            }
        };
        Ok(Some(PrepareRenameResponse::RangeWithPlaceholder {
            range: adapters::range(&doc, target.range),
            placeholder,
        }))
    }

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
        let Some(doc) = self.cached_document(&td.text_document.uri) else {
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
        let Some(target) = self.rename_target(&doc, offset)? else {
            return Ok(None);
        };
        if let RenameKind::Alias(import) = target.kind {
            return rename_alias(&doc, import, &params.new_name).map(Some);
        }
        if let RenameKind::Binding(symbol) = target.kind {
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
        let RenameKind::Symbol { uri, name } = target.kind else {
            return Ok(None);
        };
        let mut changes = HashMap::<Url, Vec<TextEdit>>::new();
        for location in self.symbol_references(&uri, &name, true) {
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
