use super::super::{adapters, features::code_end_offset, lsp::Backend};
use std::collections::HashMap;
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
        NumberOrString, Range, TextEdit, WorkspaceEdit,
    },
};

impl Backend {
    pub(in crate::server) async fn handle_code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<CodeActionResponse>> {
        let _workspace_read = self.document_read(&params.text_document.uri).await;
        let uri = params.text_document.uri;
        let Some(doc) = self.document(&uri) else {
            return Ok(None);
        };
        if !Self::supported(&doc) {
            return Ok(None);
        }
        if params
            .context
            .only
            .as_ref()
            .is_some_and(|only| !only.iter().any(|kind| kind == &CodeActionKind::QUICKFIX))
        {
            return Ok(Some(Vec::new()));
        }
        let requested_range = params.range;
        let mut actions = Vec::new();
        for diagnostic in params.context.diagnostics {
            if (diagnostic.range.end.line, diagnostic.range.end.character)
                < (requested_range.start.line, requested_range.start.character)
                || (requested_range.end.line, requested_range.end.character)
                    < (
                        diagnostic.range.start.line,
                        diagnostic.range.start.character,
                    )
            {
                continue;
            }
            if diagnostic.code != Some(NumberOrString::String("parsing".into())) {
                continue;
            }
            let Some(open) = diagnostic
                .message
                .strip_prefix("Unclosed '")
                .and_then(|message| message.chars().next())
            else {
                continue;
            };
            let Some(close) = (match open {
                '(' => Some(')'),
                '[' => Some(']'),
                '{' => Some('}'),
                _ => None,
            }) else {
                continue;
            };
            let insertion = adapters::position_at(
                &doc,
                code_end_offset(&doc, adapters::offset_at(&doc, diagnostic.range.start)),
            );
            let edit = WorkspaceEdit {
                changes: Some(HashMap::from([(
                    uri.clone(),
                    vec![TextEdit {
                        range: Range::new(insertion, insertion),
                        new_text: close.to_string(),
                    }],
                )])),
                ..Default::default()
            };
            actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Insert '{close}'"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic]),
                edit: Some(edit),
                is_preferred: Some(true),
                ..Default::default()
            }));
        }
        Ok(Some(actions))
    }
}
