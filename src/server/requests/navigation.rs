use super::super::{
    adapters,
    features::{cursor_in_comment_or_string, static_hover, token_at},
    lsp::Backend,
};
use crate::analysis;
use crate::workspace::{Document, SemanticTarget};
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        DocumentLink, DocumentLinkParams, GotoDefinitionParams, GotoDefinitionResponse, Hover,
        HoverContents, HoverParams, Location, MarkupContent, MarkupKind, Position, Range,
    },
};
use url::Url;

impl Backend {
    pub(in crate::server) fn available_import_targets(&self, document: &Document) -> Vec<String> {
        self.workspace
            .read()
            .available_import_targets(&document.uri)
    }

    async fn document_read_with_editor_links(
        &self,
        uri: &Url,
        position: Option<Position>,
    ) -> tokio::sync::RwLockReadGuard<'_, ()> {
        loop {
            let read = self.document_read_with_prelude(uri).await;
            let editor_paths_ready = self.workspace.read().editor_import_paths_ready(uri);
            if !editor_paths_ready {
                drop(read);
                self.prepare_editor_import_paths(uri).await;
                continue;
            }
            if !self.workspace.discovery_pending() {
                return read;
            }
            let needs_editor_index = {
                let database = self.workspace.read();
                database.cached_document(uri).is_some_and(|document| {
                    let offset = position.map(|position| adapters::offset_at(&document, position));
                    document.syntax.imports().iter().any(|import| {
                        import.path_text(&document.text) != "Base"
                            && database.import_target(uri, import.path).is_none()
                            && offset.is_none_or(|offset| {
                                import.path.contains(offset)
                                    || import.alias.is_some_and(|alias| alias.contains(offset))
                            })
                    })
                })
            };
            if !needs_editor_index {
                return read;
            }
            // These are uncertain editor links only. Discovery never adds
            // compiler edges for extensionless or non-native module paths.
            drop(read);
            self.workspace.wait_for_discovery_worker().await;
        }
    }

    fn import_uri(
        &self,
        document: &Document,
        import: &analysis::IndexedImport,
    ) -> Option<(Url, bool)> {
        tracing::info_span!("navigation.module_lookup").in_scope(|| {
            if import.path_text(&document.text) == "Base" {
                return (import.valid_syntax && import.alias.is_none())
                    .then(|| self.prelude_module(document))
                    .flatten()
                    .map(|module| (module.uri, false));
            }
            let database = self.workspace.read();
            if import.valid_syntax
                && let Some(uri) = database.import_logical_uri(&document.uri, import.path)
            {
                return Some((uri, false));
            }
            database
                .editor_import_uri(&document.uri, import.path)
                .map(|uri| (uri, true))
        })
    }

    fn import_definition(
        &self,
        document: &Document,
        import: &analysis::IndexedImport,
    ) -> Option<GotoDefinitionResponse> {
        let (uri, _) = self.import_uri(document, import)?;
        Some(GotoDefinitionResponse::Scalar(Location {
            uri,
            range: Range::default(),
        }))
    }

    fn module_uri_at(&self, document: &Document, offset: usize) -> Option<Url> {
        let id = document.syntax.token_at_or_before(offset)?;
        let token = document.syntax.token(id)?;
        if token.kind != analysis::TokenKind::Identifier
            || !token.range.contains(offset)
            || document.syntax.symbol_for_token(id).is_some()
            || id.0.checked_sub(1).is_some_and(|previous| {
                document
                    .syntax
                    .token_text(&document.text, analysis::TokenId(previous))
                    == Some(".")
            })
        {
            return None;
        }
        if self.semantic_view(&document.uri).is_some_and(|view| {
            view.file_id(&document.uri)
                .and_then(|file| view.resolve_at(file, offset))
                .is_some_and(|occurrence| matches!(occurrence.target, SemanticTarget::Local(_)))
        }) {
            return None;
        }
        let alias = document.syntax.token_text(&document.text, id)?;
        document
            .syntax
            .imports()
            .iter()
            .find(|import| import.alias_text(&document.text) == Some(alias))
            .and_then(|import| self.import_uri(document, import).map(|(uri, _)| uri))
            .or_else(|| self.module_document(document, alias).map(|(uri, _)| uri))
    }

    pub(in crate::server) async fn handle_document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let _workspace_read = self
            .document_read_with_editor_links(&params.text_document.uri, None)
            .await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let mut links = Vec::new();
        for import in analysis::imports(&doc) {
            let path = import.path_text(&doc.text);
            if let Some((target, uncertain)) = self.import_uri(&doc, import) {
                links.push(DocumentLink {
                    range: adapters::range(&doc, import.path),
                    target: Some(target),
                    tooltip: Some(if uncertain {
                        format!("Open {path} (editor path; not compiler-resolved)")
                    } else {
                        format!("Open {path}")
                    }),
                    data: uncertain
                        .then(|| serde_json::json!({"uncertain":true,"resolution":"editor-path"})),
                });
            }
        }
        Ok(Some(links))
    }

    pub(in crate::server) async fn handle_hover(
        &self,
        params: HoverParams,
    ) -> Result<Option<Hover>> {
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
        let selection = self.semantic_selection(&doc, offset);
        if let Some(SemanticTarget::Local(binding)) = selection
            .as_ref()
            .map(|selection| selection.occurrence.target)
        {
            let symbol = binding.symbol;
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
        let value = selection
            .and_then(|selection| {
                let SemanticTarget::Global(id) = selection.occurrence.target else {
                    return None;
                };
                let entity = selection.view.entity(id)?;
                let (source, symbol) =
                    Self::semantic_symbol(&selection.view, entity.signature_owner)?;
                analysis::declaration_hover_by_id(&source, symbol)
            })
            .or_else(|| {
                if cursor_in_comment_or_string(&doc, offset) {
                    return None;
                }
                static_hover(token_at(&doc, offset))
            });
        let Some(value) = value else {
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

    pub(in crate::server) async fn handle_goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let _workspace_read = self
            .document_read_with_editor_links(
                &params.text_document_position_params.text_document.uri,
                Some(params.text_document_position_params.position),
            )
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
        let (offset, in_comment_or_string) = tracing::info_span!("navigation.cursor_lookup")
            .in_scope(|| {
                let offset = adapters::offset_at(&doc, td.position);
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
            return Ok(self.import_definition(&doc, import));
        }
        let module_uri = self.module_uri_at(&doc, offset);
        if let Some(uri) = module_uri {
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri,
                range: Range::default(),
            })));
        }
        let Some(selection) = self.semantic_selection(&doc, offset) else {
            return Ok(None);
        };
        let target = selection.occurrence.target;
        if !matches!(target, SemanticTarget::Global(_) | SemanticTarget::Local(_)) {
            return Ok(None);
        }
        let location = selection.view.target_site(target).and_then(|site| {
            let mut location = Self::semantic_location(&selection.view, site)?;
            if matches!(target, SemanticTarget::Global(_)) {
                let written = &doc.text
                    [selection.occurrence.full_range.start..selection.occurrence.full_range.end];
                if let Some((alias, _)) = written.split_once('.') {
                    let database = self.workspace.read();
                    if let Some(import) = analysis::imports(&doc)
                        .iter()
                        .find(|import| import.alias_text(&doc.text) == Some(alias))
                        && let Some(target) = database.import_target(&doc.uri, import.path)
                        && selection.view.file_id(&target.uri) == Some(site.file)
                        && let Some(uri) = database.import_logical_uri(&doc.uri, import.path)
                    {
                        location.uri = uri;
                    }
                }
            }
            Some(location)
        });
        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    pub(in crate::server) async fn handle_goto_type_definition(
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
        let offset = adapters::offset_at(&doc, td.position);
        let selection = self.semantic_selection(&doc, offset);
        if let Some(SemanticTarget::Local(binding)) = selection
            .as_ref()
            .map(|selection| selection.occurrence.target)
        {
            return Ok(self
                .semantic_binding_type(&doc, binding.symbol)
                .map(GotoDefinitionResponse::Scalar));
        }
        let Some(selection) = selection else {
            return Ok(None);
        };
        let SemanticTarget::Global(id) = selection.occurrence.target else {
            return Ok(None);
        };
        let Some(entity) = selection.view.entity(id) else {
            return Ok(None);
        };
        if entity.kind != analysis::DeclarationKind::Type {
            return Ok(None);
        }
        Ok(Self::semantic_location(&selection.view, entity.declaration)
            .map(GotoDefinitionResponse::Scalar))
    }
}
