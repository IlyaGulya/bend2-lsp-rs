use super::super::{
    adapters,
    features::{cursor_in_comment_or_string, static_hover},
    lsp::Backend,
};
use super::shared::{binding_at, declaration_token_at};
use crate::analysis;
use crate::workspace::Document;
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        DocumentLink, DocumentLinkParams, GotoDefinitionParams, GotoDefinitionResponse, Hover,
        HoverContents, HoverParams, Location, MarkupContent, MarkupKind, Range,
    },
};
use tracing::Instrument;
use url::Url;

impl Backend {
    pub(in crate::server) fn available_import_targets(&self, document: &Document) -> Vec<String> {
        self.workspace
            .read()
            .available_import_targets(&document.uri)
    }

    fn import_uri(&self, document: &Document, import: &analysis::IndexedImport) -> Option<Url> {
        tracing::info_span!("navigation.module_lookup").in_scope(|| {
            if import.path_text(&document.text) == "Base" {
                self.prelude_module(document).map(|module| module.uri)
            } else {
                self.workspace
                    .read()
                    .import_target(&document.uri, import.path)
                    .map(|target| target.uri)
            }
        })
    }

    fn import_definition(
        &self,
        document: &Document,
        import: &analysis::IndexedImport,
    ) -> Option<GotoDefinitionResponse> {
        let uri = self.import_uri(document, import)?;
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
        let alias = document.syntax.token_text(&document.text, id)?;
        self.module_document(document, alias).map(|(uri, _)| uri)
    }

    pub(in crate::server) async fn handle_document_link(
        &self,
        params: DocumentLinkParams,
    ) -> Result<Option<Vec<DocumentLink>>> {
        let _workspace_read = self
            .document_read_with_prelude(&params.text_document.uri)
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
            if let Some(target) = self.import_uri(&doc, import) {
                links.push(DocumentLink {
                    range: adapters::range(&doc, import.path),
                    target: Some(target),
                    tooltip: Some(format!("Open {path}")),
                    data: None,
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
            .document_read_with_prelude(&params.text_document_position_params.text_document.uri)
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
        let Some(token) = declaration_token_at(&doc, adapters::offset_at(&doc, td.position)) else {
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

    pub(in crate::server) async fn handle_goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let _workspace_read = self
            .document_read_with_prelude(&params.text_document_position_params.text_document.uri)
            .instrument(tracing::info_span!("navigation.ensure_prelude"))
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
                    let range = adapters::range(&doc, range);
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
                    let range = adapters::range(&module, range);
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
                    range: adapters::range(&target_text, range),
                })
            });
        Ok(Some(response))
    }

    pub(in crate::server) async fn handle_goto_type_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let _workspace_read = self
            .document_read_with_prelude(&params.text_document_position_params.text_document.uri)
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
                    range: adapters::range(&doc, range),
                })
            }));
        }
        let Some(token) = declaration_token_at(&doc, adapters::offset_at(&doc, td.position)) else {
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
            range: adapters::range(&target_text, range),
        })))
    }
}
