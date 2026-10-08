use super::super::{adapters, features::cursor_in_comment_or_string, lsp::Backend};
use super::import_edits::import_path_edit;
use crate::{analysis, workspace::Document};
use std::collections::HashSet;
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        CompletionItem, CompletionItemKind, CompletionList, CompletionParams, CompletionResponse,
        CompletionTextEdit,
        InlayHint, InlayHintParams, SignatureHelp, SignatureHelpParams,
    },
};

struct ExplicitCompletionType {
    uri: url::Url,
    snapshot: std::sync::Arc<analysis::DocumentSnapshot>,
    name: String,
    alias: Option<String>,
}

fn completion_qualifier(
    doc: &analysis::DocumentSnapshot,
    prefix_start: usize,
) -> Option<(analysis::TokenId, &str)> {
    let syntax = &doc.syntax;
    let previous = prefix_start.checked_sub(1)?;
    let dot_id = syntax.token_at_or_before(previous)?;
    let dot = syntax.token(dot_id)?;
    if syntax.token_text(&doc.text, dot_id) != Some(".") || dot.range.end != prefix_start {
        return None;
    }
    let mut root = analysis::TokenId(dot_id.0.checked_sub(1)?);
    let end = syntax.token(root)?.range.end;
    loop {
        let token = syntax.token(root)?;
        if token.kind != analysis::TokenKind::Identifier {
            return None;
        }
        let Some(previous_dot) = root.0.checked_sub(1).map(analysis::TokenId) else {
            break;
        };
        let previous = syntax.token(previous_dot)?;
        if syntax.token_text(&doc.text, previous_dot) != Some(".")
            || previous.range.end != token.range.start
        {
            break;
        }
        let previous_name = analysis::TokenId(previous_dot.0.checked_sub(1)?);
        if syntax.token(previous_name)?.range.end != previous.range.start {
            break;
        }
        root = previous_name;
    }
    let start = syntax.token(root)?.range.start;
    Some((root, &doc.text[start..end]))
}

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
        if let Some(path) = syntax.completion_import_path(offset) {
            let prefix = &doc.text[path.start..offset];
            let items = self
                .available_import_targets(&doc)
                .into_iter()
                .filter(|target| adapters::import_completion_match(target, prefix).is_some())
                .filter_map(|target| {
                    let edit = import_path_edit(&doc, path, &target)?;
                    let mut item = CompletionItem::new_simple(target, "Available import".into());
                    item.kind = Some(CompletionItemKind::MODULE);
                    item.text_edit = Some(CompletionTextEdit::Edit(edit));
                    Some(item)
                })
                .collect();
            // Import filtering and file-vs-path metadata depend on the current
            // query. A complete array lets Zed reuse stale items after typing.
            return Ok(Some(CompletionResponse::List(CompletionList {
                is_incomplete: true,
                items: adapters::finish_import_completion_items(&doc, path, prefix, items),
            })));
        }
        let prefix_token = offset
            .checked_sub(1)
            .and_then(|previous| syntax.token_at_or_before(previous))
            .and_then(|id| syntax.token(id).map(|token| (id, token)))
            .filter(|(_, token)| {
                matches!(
                    token.kind,
                    analysis::TokenKind::Identifier | analysis::TokenKind::Number
                ) && token.range.start <= offset
                    && offset <= token.range.end
            });
        let replacement = prefix_token
            .map_or(analysis::TextRange::new(offset, offset), |(_, token)| {
                token.range
            });
        let prefix = &doc.text[replacement.start..offset];
        let qualifier = completion_qualifier(&doc, replacement.start);
        let pattern = syntax.case_pattern_type(offset);
        let expected = pattern
            .and_then(|pattern| pattern.explicit_type)
            .and_then(|range| doc.text.get(range.start..range.end))
            .and_then(|name| self.explicit_completion_type(&doc, name));
        let items = if let Some((root, qualifier)) = qualifier {
            if syntax
                .symbol_for_token(root)
                .and_then(|id| syntax.binding_by_id(id))
                .is_some()
            {
                Vec::new()
            } else {
                self.qualified_context_completion_items(
                    &doc,
                    qualifier,
                    prefix,
                    pattern.is_some(),
                    expected.as_ref(),
                )
            }
        } else if let Some(expected) = expected {
            let mut items = analysis::typed_constructor_completion_items(
                &expected.snapshot,
                None,
                prefix,
                Some(&expected.name),
            );
            if let Some(alias) = expected.alias {
                for item in &mut items {
                    item.label = format!("{alias}.{}", item.label);
                }
            }
            adapters::completion_items(items)
        } else {
            self.unqualified_completion_items(&doc, replacement.start, prefix, pattern.is_some())
        };
        Ok(Some(CompletionResponse::Array(
            adapters::finish_completion_items(&doc, replacement, prefix, items),
        )))
    }

    fn qualified_context_completion_items(
        &self,
        doc: &Document,
        qualifier: &str,
        prefix: &str,
        in_case_pattern: bool,
        expected: Option<&ExplicitCompletionType>,
    ) -> Vec<CompletionItem> {
        let (alias, namespace) = qualifier
            .split_once('.')
            .map_or((qualifier, None), |(alias, rest)| (alias, Some(rest)));
        if let Some((source_uri, source)) = self.module_document(doc, alias) {
            if in_case_pattern {
                if expected.is_some_and(|target| target.uri != source_uri) {
                    return Vec::new();
                }
                adapters::completion_items(analysis::typed_constructor_completion_items(
                    &source,
                    namespace,
                    prefix,
                    expected.map(|target| target.name.as_str()),
                ))
            } else {
                adapters::completion_items(namespace.map_or_else(
                    || analysis::module_completion_items(&source, prefix),
                    |namespace| analysis::qualified_completion_items(&source, namespace, prefix),
                ))
            }
        } else {
            let source = expected
                .map(|target| target.snapshot.clone())
                .or_else(|| self.prelude_module(doc).map(|module| module.snapshot))
                .unwrap_or_else(|| doc.snapshot.clone());
            adapters::completion_items(if in_case_pattern {
                analysis::typed_constructor_completion_items(
                    &source,
                    Some(qualifier),
                    prefix,
                    expected.map(|target| target.name.as_str()),
                )
            } else {
                analysis::qualified_completion_items(&source, qualifier, prefix)
            })
        }
    }

    fn explicit_completion_type(
        &self,
        doc: &Document,
        name: &str,
    ) -> Option<ExplicitCompletionType> {
        let has_type = |source: &analysis::DocumentSnapshot, name: &str| {
            source
                .syntax
                .name_id(&source.text, name)
                .and_then(|id| source.syntax.symbol_by_name(id))
                .is_some_and(|symbol| symbol.kind == analysis::SymbolKind::Struct)
        };
        if has_type(doc, name) {
            return Some(ExplicitCompletionType {
                uri: doc.uri.clone(),
                snapshot: doc.snapshot.clone(),
                name: name.to_owned(),
                alias: None,
            });
        }
        if let Some((alias, member)) = name.split_once('.')
            && let Some((uri, source)) = self.module_document(doc, alias)
            && has_type(&source, member)
        {
            return Some(ExplicitCompletionType {
                uri,
                snapshot: source,
                name: member.to_owned(),
                alias: Some(alias.to_owned()),
            });
        }
        let source = self.prelude_module(doc)?;
        has_type(&source, name).then(|| ExplicitCompletionType {
            uri: source.uri,
            snapshot: source.snapshot,
            name: name.to_owned(),
            alias: None,
        })
    }

    fn unqualified_completion_items(
        &self,
        doc: &Document,
        prefix_start: usize,
        prefix: &str,
        in_case_pattern: bool,
    ) -> Vec<CompletionItem> {
        let mut items = adapters::completion_items(if in_case_pattern {
            analysis::constructor_completion_items(doc, None, prefix)
        } else {
            analysis::scoped_completion_items(doc, prefix_start, prefix)
        });
        let mut labels: HashSet<String> = items.iter().map(|item| item.label.clone()).collect();
        if let Some(module) = self.prelude_module(doc) {
            items.extend(
                adapters::completion_items(if in_case_pattern {
                    analysis::constructor_completion_items(&module, None, prefix)
                } else {
                    analysis::completion_items(&module, prefix)
                })
                .into_iter()
                .filter(|item| labels.insert(item.label.clone())),
            );
        }
        if !in_case_pattern {
            for import in analysis::imports(doc) {
                if let Some(alias) = import
                    .alias_text(&doc.text)
                    .filter(|alias| analysis::completion_match(alias, prefix).is_some())
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
