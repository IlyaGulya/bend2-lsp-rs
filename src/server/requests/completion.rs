use super::super::{adapters, features::cursor_in_comment_or_string, lsp::Backend};
use super::import_edits::{auto_import_plan, import_path_edit};
use crate::{
    analysis,
    workspace::{Document, SemanticTarget},
};
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        CompletionItem, CompletionItemKind, CompletionItemLabelDetails, CompletionList,
        CompletionParams, CompletionResponse, CompletionTextEdit, InlayHint, InlayHintParams,
        SignatureHelp, SignatureHelpParams,
    },
};

impl Backend {
    pub(in crate::server) async fn handle_completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
        let discovery_incomplete = {
            // Preserve preceding watcher updates without waiting for the
            // background initial scan. Local bindings are already indexed.
            let _serial = self.workspace.update_serial.lock().await;
            self.workspace.discovery_pending()
        };
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
                    let editor_only = !target.is_ascii();
                    let mut item = CompletionItem::new_simple(
                        target,
                        if editor_only {
                            "Editor path (not a compiler-valid module path)".into()
                        } else {
                            "Available import".into()
                        },
                    );
                    if editor_only {
                        item.label_details = Some(CompletionItemLabelDetails {
                            detail: None,
                            description: Some("editor-only".into()),
                        });
                        item.data =
                            Some(serde_json::json!({"uncertain":true,"resolution":"editor-path"}));
                    }
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
        let replacement = offset
            .checked_sub(1)
            .and_then(|previous| syntax.token_at_or_before(previous))
            .and_then(|id| syntax.token(id))
            .filter(|token| {
                matches!(
                    token.kind,
                    analysis::TokenKind::Identifier | analysis::TokenKind::Number
                ) && token.range.start <= offset
                    && offset <= token.range.end
            })
            .map_or(analysis::TextRange::new(offset, offset), |token| {
                token.range
            });
        let prefix = &doc.text[replacement.start..offset];
        let (mut items, known_pattern_type) = self.semantic_completion_items(&doc, offset);
        // Auto-import suggestions are prospective edits, not name-resolution
        // fallbacks. Their candidates come from immutable workspace indexes.
        let qualified = replacement
            .start
            .checked_sub(1)
            .is_some_and(|previous| doc.text.as_bytes().get(previous) == Some(&b'.'));
        if !qualified && !known_pattern_type {
            self.append_import_completion_items(
                &doc,
                replacement.start,
                prefix,
                syntax.case_pattern_type(offset).is_some(),
                &mut items,
            );
        }
        Ok(Some(CompletionResponse::List(CompletionList {
            is_incomplete: discovery_incomplete,
            items: adapters::finish_completion_items(&doc, replacement, prefix, items),
        })))
    }

    fn append_import_completion_items(
        &self,
        doc: &Document,
        offset: usize,
        prefix: &str,
        in_case_pattern: bool,
        items: &mut Vec<CompletionItem>,
    ) {
        let workspace = self.workspace.read();
        let candidates = workspace.import_completion_candidates(&doc.uri, prefix, in_case_pattern);
        let base = self.compiler.base_module.read().clone().filter(|_| {
            !doc.syntax.imports().iter().any(|import| {
                import.valid_syntax
                    && import.alias.is_none()
                    && import.path_text(&doc.text) == "Base"
            })
        });
        let base_candidates = base.map(|module| {
            let completions = if in_case_pattern {
                analysis::constructor_completion_items(&module.snapshot, None, prefix)
            } else {
                analysis::module_completion_items(&module.snapshot, prefix)
            };
            (
                Document::with_snapshot(module.uri, "bend".into(), module.snapshot),
                "Base".to_owned(),
                completions,
            )
        });
        for (target, path, completions) in candidates.into_iter().chain(base_candidates) {
            for mut item in adapters::completion_items(completions) {
                let Some(plan) =
                    auto_import_plan(doc, &workspace, &target.uri, &path, &item.label, offset)
                else {
                    continue;
                };
                let detail = item.detail.get_or_insert_with(String::new);
                detail.push_str(" — from ");
                detail.push_str(&path);
                item.label_details = Some(CompletionItemLabelDetails {
                    detail: None,
                    description: Some(path.clone()),
                });
                item.insert_text = Some(plan.qualified_name);
                item.additional_text_edits = plan.import_edit.map(|edit| vec![edit]);
                item.sort_text = adapters::completion_sort_text(&item, prefix, true);
                items.push(item);
            }
        }
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
        let offset = adapters::offset_at(&doc, td.position);
        let Some((_, active, _, token)) = doc.syntax.active_call(&doc.text, offset) else {
            return Ok(None);
        };
        let Some(range) = doc.syntax.token(token).map(|token| token.range) else {
            return Ok(None);
        };
        let Some(selection) = self.semantic_selection(&doc, range.start) else {
            return Ok(None);
        };
        let SemanticTarget::Global(id) = selection.occurrence.target else {
            return Ok(None);
        };
        let Some(entity) = selection.view.entity(id) else {
            return Ok(None);
        };
        let Some((source, symbol)) = Self::semantic_symbol(&selection.view, entity.signature_owner)
        else {
            return Ok(None);
        };
        Ok(
            analysis::signature_help_for_symbol(&source, symbol, active as usize)
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
        let hints = self.semantic_inlay_hints(&doc, range);
        Ok(Some(adapters::inlay_hints(&doc, hints)))
    }
}
