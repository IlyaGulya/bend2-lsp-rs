use super::super::{adapters, features::cursor_in_comment_or_string, lsp::Backend};
use crate::{analysis, workspace::Document};
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
                    .then(|| (alias_id, &doc.text[alias.range.start..alias.range.end]))
            });
        let pattern_start = alias.map_or(prefix_start, |(id, _)| {
            syntax
                .token(id)
                .map_or(prefix_start, |token| token.range.start)
        });
        let in_case_pattern = syntax
            .line_code_range(td.position.line as usize)
            .and_then(|line| syntax.token_at_or_before(line.start))
            .and_then(|id| syntax.token(id).map(|token| (id, token)))
            .is_some_and(|(id, token)| {
                syntax.token_text(&doc.text, id) == Some("case")
                    && token.range.end < pattern_start
                    && doc.text[token.range.end..pattern_start]
                        .bytes()
                        .all(|byte| matches!(byte, b' ' | b'\t'))
            });
        if let Some((alias_id, alias)) = alias {
            if syntax
                .symbol_for_token(alias_id)
                .and_then(|symbol| syntax.binding_by_id(symbol))
                .is_some()
            {
                return Ok(Some(CompletionResponse::Array(Vec::new())));
            }
            if let Some((_, source)) = self.module_document(&doc, alias) {
                return Ok(Some(CompletionResponse::Array(adapters::completion_items(
                    if in_case_pattern {
                        analysis::constructor_completion_items(&source, None, prefix)
                    } else {
                        analysis::module_completion_items(&source, prefix)
                    },
                ))));
            }
            if let Some(module) = self.prelude_module(&doc) {
                return Ok(Some(CompletionResponse::Array(adapters::completion_items(
                    if in_case_pattern {
                        analysis::constructor_completion_items(&module, Some(alias), prefix)
                    } else {
                        analysis::qualified_completion_items(&module, alias, prefix)
                    },
                ))));
            }
            if in_case_pattern {
                return Ok(Some(CompletionResponse::Array(Vec::new())));
            }
        }
        Ok(Some(CompletionResponse::Array(
            self.unqualified_completion_items(&doc, prefix_start, prefix, in_case_pattern),
        )))
    }

    fn unqualified_completion_items(
        &self,
        doc: &Document,
        prefix_start: usize,
        prefix: &str,
        in_case_pattern: bool,
    ) -> Vec<CompletionItem> {
        if in_case_pattern {
            let mut items = adapters::completion_items(analysis::constructor_completion_items(
                doc, None, prefix,
            ));
            if let Some(module) = self.prelude_module(doc) {
                let mut labels: HashSet<String> =
                    items.iter().map(|item| item.label.clone()).collect();
                items.extend(
                    adapters::completion_items(analysis::constructor_completion_items(
                        &module, None, prefix,
                    ))
                    .into_iter()
                    .filter(|item| labels.insert(item.label.clone())),
                );
            }
            return items;
        }
        let mut items = adapters::completion_items(analysis::scoped_completion_items(
            doc,
            prefix_start,
            prefix,
        ));
        if !prefix.is_empty() {
            let mut labels: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();
            if let Some(module) = self.prelude_module(doc) {
                items.extend(
                    adapters::completion_items(analysis::completion_items(&module, prefix))
                        .into_iter()
                        .filter(|item| labels.insert(item.label.clone())),
                );
            }
            for import in analysis::imports(doc) {
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
        items
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
