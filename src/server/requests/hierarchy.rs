use super::super::{
    adapters,
    features::{
        call_hierarchy_item, cursor_in_comment_or_string, type_hierarchy_item,
        type_hierarchy_symbol,
    },
    lsp::Backend,
};
use super::shared::declaration_token_at;
use crate::analysis;
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
        CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
        SymbolKind, TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
        TypeHierarchySupertypesParams,
    },
};

impl Backend {
    pub(in crate::server) async fn handle_prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
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
        let database = self.workspace.read();
        let resolved = selected
            .and_then(|token| doc.syntax.symbol_for_token(token))
            .filter(|symbol| {
                doc.syntax
                    .symbol_by_id(*symbol)
                    .is_some_and(|symbol| symbol.kind == analysis::SymbolKind::Function)
            })
            .and_then(|symbol| database.global_symbol_id(&doc.uri, symbol))
            .and_then(|id| database.symbol_by_id(id))
            .or_else(|| selected.and_then(|token| database.resolve_call(&doc.uri, token)));
        let Some(resolved) = resolved else {
            return Ok(None);
        };
        let Some(symbol) =
            adapters::document_symbol_by_id(&resolved.document, resolved.id.local_symbol())
        else {
            return Ok(None);
        };
        Ok(Some(vec![call_hierarchy_item(
            resolved.document.uri,
            symbol,
        )]))
    }

    pub(in crate::server) async fn handle_incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let _workspace_read = self.workspace_ready_read().await;
        let database = self.workspace.read();
        let calls = database
            .symbol_by_name(&params.item.uri, &params.item.name)
            .map_or_else(Vec::new, |symbol| database.incoming_calls(symbol.id));
        Ok(Some(
            calls
                .into_iter()
                .filter_map(|group| {
                    let symbol = adapters::document_symbol_by_id(
                        &group.symbol.document,
                        group.symbol.id.local_symbol(),
                    )?;
                    Some(CallHierarchyIncomingCall {
                        from: call_hierarchy_item(group.symbol.document.uri, symbol),
                        from_ranges: group
                            .ranges
                            .into_iter()
                            .map(|range| adapters::range(&group.source, range))
                            .collect(),
                    })
                })
                .collect(),
        ))
    }

    pub(in crate::server) async fn handle_outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let _workspace_read = self
            .workspace_read_with_prelude(Some(&params.item.uri))
            .await;
        if self.hierarchy_document(&params.item.uri).is_none() {
            return Ok(Some(Vec::new()));
        }
        let database = self.workspace.read();
        let calls = database
            .symbol_by_name(&params.item.uri, &params.item.name)
            .map_or_else(Vec::new, |symbol| database.outgoing_calls(symbol.id));
        Ok(Some(
            calls
                .into_iter()
                .filter_map(|group| {
                    let symbol = adapters::document_symbol_by_id(
                        &group.symbol.document,
                        group.symbol.id.local_symbol(),
                    )?;
                    Some(CallHierarchyOutgoingCall {
                        to: call_hierarchy_item(group.symbol.document.uri, symbol),
                        from_ranges: group
                            .ranges
                            .into_iter()
                            .map(|range| adapters::range(&group.source, range))
                            .collect(),
                    })
                })
                .collect(),
        ))
    }

    pub(in crate::server) async fn handle_prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
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

    pub(in crate::server) async fn handle_supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let _workspace_read = self.document_read(&params.item.uri).await;
        let item = params.item;
        let Some(doc) = self.hierarchy_document(&item.uri) else {
            return Ok(Some(Vec::new()));
        };
        let parent = adapters::document_symbols(&doc).into_iter().find(|symbol| {
            symbol.kind == SymbolKind::STRUCT
                && symbol
                    .children
                    .as_ref()
                    .is_some_and(|children| children.iter().any(|child| child.name == item.name))
        });
        Ok(Some(
            parent
                .map(|symbol| vec![type_hierarchy_item(doc.uri, symbol)])
                .unwrap_or_default(),
        ))
    }

    pub(in crate::server) async fn handle_subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let _workspace_read = self.document_read(&params.item.uri).await;
        let item = params.item;
        let Some(doc) = self.hierarchy_document(&item.uri) else {
            return Ok(Some(Vec::new()));
        };
        let children = adapters::document_symbols(&doc)
            .into_iter()
            .find(|symbol| symbol.kind == SymbolKind::STRUCT && symbol.name == item.name)
            .and_then(|symbol| symbol.children)
            .unwrap_or_default()
            .into_iter()
            .map(|symbol| type_hierarchy_item(doc.uri.clone(), symbol))
            .collect();
        Ok(Some(children))
    }
}
