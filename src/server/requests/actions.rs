use super::super::{adapters, features::code_end_offset, lsp::Backend};
use super::import_edits::{
    auto_import_edits, organize_imports, unresolved_reference, versioned_edit,
};
use crate::{analysis, workspace::Document};
use tower_lsp::{
    jsonrpc::Result,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
        NumberOrString, Position, Range, TextEdit,
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
        let requested_range = params.range;
        let mut actions = Vec::new();
        if kind_requested(
            params.context.only.as_deref(),
            &CodeActionKind::SOURCE_ORGANIZE_IMPORTS,
        ) {
            let edits = organize_imports(&doc, &self.workspace.read());
            if !edits.is_empty() {
                actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                    title: "Organize imports".into(),
                    kind: Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
                    edit: Some(versioned_edit(&doc, edits)),
                    ..Default::default()
                }));
            }
        }
        if !kind_requested(params.context.only.as_deref(), &CodeActionKind::QUICKFIX) {
            return Ok(Some(actions));
        }
        self.append_auto_import_actions(&doc, requested_range.start, &mut actions);
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
            let edit = versioned_edit(
                &doc,
                vec![TextEdit {
                    range: Range::new(insertion, insertion),
                    new_text: close.to_string(),
                }],
            );
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

    fn append_auto_import_actions(
        &self,
        doc: &Document,
        position: Position,
        actions: &mut Vec<CodeActionOrCommand>,
    ) {
        if let Some(reference) = unresolved_reference(doc, adapters::offset_at(doc, position)) {
            let name = doc.syntax.name_text(&doc.text, reference.name);
            if reference.qualifier.is_some() || self.prelude_declaration(doc, name).is_none() {
                let workspace = self.workspace.read();
                let base = self
                    .compiler
                    .base_module
                    .read()
                    .as_ref()
                    .filter(|module| analysis::declaration_range(&module.snapshot, name).is_some())
                    .map(|module| (module.uri.clone(), "Base".to_owned()));
                let candidates = workspace
                    .import_candidates(&doc.uri, name)
                    .into_iter()
                    .map(|(target, path)| (target.uri, path))
                    .chain(base);
                for (target, path) in candidates {
                    if let Some(edits) =
                        auto_import_edits(doc, &workspace, &target, &path, reference)
                    {
                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            title: format!("Import {name} from {path}"),
                            kind: Some(CodeActionKind::QUICKFIX),
                            edit: Some(versioned_edit(doc, edits)),
                            ..Default::default()
                        }));
                    }
                }
            }
        }
    }
}

fn kind_requested(only: Option<&[CodeActionKind]>, kind: &CodeActionKind) -> bool {
    only.is_none_or(|only| {
        only.iter().any(|requested| {
            let requested = requested.as_str();
            let kind = kind.as_str();
            requested.is_empty()
                || kind == requested
                || kind
                    .strip_prefix(requested)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        })
    })
}
