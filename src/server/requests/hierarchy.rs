use super::super::{adapters, lsp::Backend};
use crate::{analysis, workspace::SemanticTarget};
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        CallHierarchyIncomingCall, CallHierarchyIncomingCallsParams, CallHierarchyItem,
        CallHierarchyOutgoingCall, CallHierarchyOutgoingCallsParams, CallHierarchyPrepareParams,
        TypeHierarchyItem, TypeHierarchyPrepareParams, TypeHierarchySubtypesParams,
        TypeHierarchySupertypesParams,
    },
};

impl Backend {
    pub(in crate::server) async fn handle_prepare_call_hierarchy(
        &self,
        params: CallHierarchyPrepareParams,
    ) -> Result<Option<Vec<CallHierarchyItem>>> {
        self.workspace.wait_for_discovery().await;
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
        let SemanticTarget::Global(entity) = selection.occurrence.target else {
            return Ok(None);
        };
        Ok(Self::semantic_hierarchy_item(&selection.view, entity).map(|item| vec![item]))
    }

    pub(in crate::server) async fn handle_incoming_calls(
        &self,
        params: CallHierarchyIncomingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyIncomingCall>>> {
        let _workspace_read = self.semantic_incoming_read(&params.item).await?;
        Ok(Some(self.semantic_incoming(&params.item)?))
    }

    pub(in crate::server) async fn handle_outgoing_calls(
        &self,
        params: CallHierarchyOutgoingCallsParams,
    ) -> Result<Option<Vec<CallHierarchyOutgoingCall>>> {
        let _workspace_read = self.workspace_ready_read().await;
        Ok(Some(self.semantic_outgoing(&params.item)?))
    }

    pub(in crate::server) async fn handle_prepare_type_hierarchy(
        &self,
        params: TypeHierarchyPrepareParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        self.workspace.wait_for_discovery().await;
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
        let SemanticTarget::Global(entity) = selection.occurrence.target else {
            return Ok(None);
        };
        let Some(entry) = selection.view.entity(entity) else {
            return Ok(None);
        };
        if !matches!(
            entry.kind,
            analysis::DeclarationKind::Type | analysis::DeclarationKind::Constructor
        ) {
            return Ok(None);
        }
        Ok(Self::semantic_type_item(&selection.view, entity).map(|item| vec![item]))
    }

    pub(in crate::server) async fn handle_supertypes(
        &self,
        params: TypeHierarchySupertypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let _workspace_read = self.document_read(&params.item.uri).await;
        let (view, entity) = self.semantic_type_target(&params.item)?;
        let Some(entry) = view.entity(entity) else {
            return Ok(Some(Vec::new()));
        };
        let Some((document, symbol)) = Self::semantic_symbol(&view, entry.declaration) else {
            return Ok(Some(Vec::new()));
        };
        let parent = document
            .syntax
            .constructor_by_symbol(symbol)
            .and_then(|constructor| view.declaration(entry.declaration.file, constructor.parent))
            .and_then(|declaration| match declaration.target {
                SemanticTarget::Global(parent) => Self::semantic_type_item(&view, parent),
                _ => None,
            });
        Ok(Some(parent.into_iter().collect()))
    }

    pub(in crate::server) async fn handle_subtypes(
        &self,
        params: TypeHierarchySubtypesParams,
    ) -> Result<Option<Vec<TypeHierarchyItem>>> {
        let _workspace_read = self.document_read(&params.item.uri).await;
        let (view, entity) = self.semantic_type_target(&params.item)?;
        let Some(entry) = view.entity(entity) else {
            return Ok(Some(Vec::new()));
        };
        let Some((document, symbol)) = Self::semantic_symbol(&view, entry.declaration) else {
            return Ok(Some(Vec::new()));
        };
        let children = document
            .syntax
            .constructors(symbol)
            .iter()
            .filter_map(|constructor| {
                let declaration = view.declaration(entry.declaration.file, constructor.symbol)?;
                let SemanticTarget::Global(child) = declaration.target else {
                    return None;
                };
                Self::semantic_type_item(&view, child)
            })
            .collect();
        Ok(Some(children))
    }
}
