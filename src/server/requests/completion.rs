use super::super::{adapters, features::cursor_in_comment_or_string, lsp::Backend};
use crate::analysis;
use std::collections::HashSet;
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        CompletionItem, CompletionItemKind, CompletionParams, CompletionResponse, InlayHint,
        InlayHintParams, SignatureHelp, SignatureHelpParams,
    },
};

impl Backend {
    pub(in crate::server) async fn handle_completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let _workspace_read = self
            .document_read_with_prelude(&params.text_document_position.text_document.uri)
            .await;
        let td = params.text_document_position;
        let Some(doc) = self.document(&td.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let offset = adapters::offset_at(&doc, td.position);
        if cursor_in_comment_or_string(&doc, offset) {
            return Ok(Some(CompletionResponse::Array(Vec::new())));
        }
        let syntax = &doc.syntax;
        let prefix_token = syntax
            .token_at_or_before(offset)
            .and_then(|id| syntax.token(id))
            .filter(|token| {
                matches!(
                    token.kind,
                    analysis::TokenKind::Identifier | analysis::TokenKind::Number
                ) && token.range.start <= offset
                    && offset <= token.range.end
            });
        let prefix_start = prefix_token.map_or(offset, |token| token.range.start);
        let prefix = &doc.text[prefix_start..offset];
        let alias = prefix_start
            .checked_sub(1)
            .and_then(|position| syntax.token_at_or_before(position))
            .and_then(|dot_id| {
                let dot = syntax.token(dot_id)?;
                if dot.kind != analysis::TokenKind::Punctuation
                    || syntax.token_text(&doc.text, dot_id) != Some(".")
                    || dot.range.end != prefix_start
                {
                    return None;
                }
                let alias_id = analysis::TokenId(dot_id.0.checked_sub(1)?);
                let alias = syntax.token(alias_id)?;
                (alias.kind == analysis::TokenKind::Identifier
                    && alias.range.end == dot.range.start)
                    .then(|| &doc.text[alias.range.start..alias.range.end])
            });
        if let Some(alias) = alias {
            if let Some((_, source)) = self.module_document(&doc, alias) {
                return Ok(Some(CompletionResponse::Array(adapters::completion_items(
                    analysis::module_completion_items(&source, prefix),
                ))));
            }
            if let Some(module) = self.prelude_module(&doc) {
                return Ok(Some(CompletionResponse::Array(adapters::completion_items(
                    analysis::qualified_completion_items(&module, alias, prefix),
                ))));
            }
        }
        let mut items = adapters::completion_items(analysis::completion_items(&doc, prefix));
        if !prefix.is_empty() {
            let mut labels: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();
            if let Some(module) = self.prelude_module(&doc) {
                items.extend(
                    adapters::completion_items(analysis::completion_items(&module, prefix))
                        .into_iter()
                        .filter(|item| labels.insert(item.label.clone())),
                );
            }
            for import in analysis::imports(&doc) {
                if let Some(alias) = import
                    .alias_text(&doc.text)
                    .filter(|alias| alias.starts_with(prefix))
                    && labels.insert(alias.to_owned())
                {
                    let mut item = CompletionItem::new_simple(
                        alias.to_owned(),
                        format!("Imported module {}", import.path_text(&doc.text)),
                    );
                    item.kind = Some(CompletionItemKind::MODULE);
                    items.push(item);
                }
            }
        }
        Ok(Some(CompletionResponse::Array(items)))
    }

    pub(in crate::server) async fn handle_signature_help(
        &self,
        params: SignatureHelpParams,
    ) -> Result<Option<SignatureHelp>> {
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
        Ok(
            analysis::signature_help(&doc, adapters::offset_at(&doc, td.position))
                .map(adapters::signature_help),
        )
    }

    pub(in crate::server) async fn handle_inlay_hint(
        &self,
        params: InlayHintParams,
    ) -> Result<Option<Vec<InlayHint>>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let Some(doc) = self.document(&params.text_document.uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        let range = adapters::text_range(&doc, params.range);
        let hints = analysis::inlay_hints(&doc, range);
        Ok(Some(adapters::inlay_hints(&doc, hints)))
    }
}
