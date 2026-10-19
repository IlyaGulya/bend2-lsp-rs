use super::super::lsp::Backend;
use super::{
    LintResult,
    wire::{Fix, offsets},
};
use crate::workspace::Document;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, CodeActionTriggerKind,
    DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier, TextDocumentEdit, TextEdit,
    WorkspaceEdit,
};

pub(in crate::server) const DANGEROUS_KIND: &str = "quickfix.bend-lint.dangerous";

fn requested(params: &CodeActionParams) -> bool {
    params.context.trigger_kind == Some(CodeActionTriggerKind::INVOKED)
        && params.context.only.as_ref().is_none_or(|kinds| {
            kinds.iter().any(|kind| {
                let kind = kind.as_str();
                kind.is_empty()
                    || kind == DANGEROUS_KIND
                    || DANGEROUS_KIND
                        .strip_prefix(kind)
                        .is_some_and(|tail| tail.starts_with('.'))
            })
        })
}

fn versioned_fix(backend: &Backend, result: &LintResult, fix: &Fix) -> Option<WorkspaceEdit> {
    if fix.applicability != "dangerous" || fix.edits.is_empty() || fix.edits.len() > 4096 {
        return None;
    }
    let mut edits = BTreeMap::<PathBuf, (Document, Vec<(usize, usize, TextEdit)>)>::new();
    for edit in &fix.edits {
        let (path, saved) = result.saved.source(&edit.path)?;
        let uri = result.saved.uris.get(&path)?;
        // Closed targets have no document version. Refuse rather than give an
        // unversioned edit that could overwrite a later disk/editor change.
        let document = backend.document(uri)?;
        if document.text != saved.text
            || !backend.diagnostics_snapshot_current(uri, &document.snapshot)
        {
            return None;
        }
        let (start, end) = offsets(saved, edit.range)?;
        edits
            .entry(path)
            .or_insert_with(|| (document, Vec::new()))
            .1
            .push((
                start,
                end,
                TextEdit {
                    range: edit.range,
                    new_text: edit.text.clone(),
                },
            ));
    }
    let mut changes = Vec::with_capacity(edits.len());
    for (_, (document, mut edits)) in edits {
        edits.sort_unstable_by_key(|(start, end, _)| (*start, *end));
        if edits.windows(2).any(|pair| {
            let (start, end, _) = &pair[0];
            let (next, next_end, _) = &pair[1];
            next < end || (next == end && (start == end || next == next_end))
        }) {
            return None;
        }
        let version = document.revision.0;
        changes.push(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: document.uri,
                version: Some(version),
            },
            edits: edits
                .into_iter()
                .map(|(_, _, edit)| OneOf::Left(edit))
                .collect(),
        });
    }
    Some(WorkspaceEdit {
        document_changes: Some(DocumentChanges::Edits(changes)),
        ..Default::default()
    })
}

impl Backend {
    pub(in crate::server) async fn lint_code_actions(
        &self,
        params: &CodeActionParams,
    ) -> Vec<CodeActionOrCommand> {
        if !requested(params) {
            return Vec::new();
        }
        let Some(result) = self
            .current_lint_result()
            .filter(|result| result.config.dangerous_code_actions)
        else {
            return Vec::new();
        };
        let saved = result.saved.clone();
        let documents = self.workspace_documents();
        let Ok(permit) = self
            .diagnostics
            .lint
            .semaphore
            .clone()
            .acquire_owned()
            .await
        else {
            return Vec::new();
        };
        let fresh = super::super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                saved.unchanged() && super::saved::SavedProject::documents_saved(&documents)
            })
            .await,
        );
        if !fresh
            || self
                .current_lint_result()
                .is_none_or(|current| !Arc::ptr_eq(&current, &result))
        {
            return Vec::new();
        }
        let mut actions = Vec::new();
        for finding in &result.findings.actionable {
            if finding.uri != params.text_document.uri
                || finding.diagnostic.range.end < params.range.start
                || params.range.end < finding.diagnostic.range.start
            {
                continue;
            }
            for fix in &finding.fixes {
                if let Some(edit) = versioned_fix(self, &result, fix) {
                    actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                        title: format!(
                            "Dangerous bend-lint fix (may change behavior): {}",
                            fix.title
                        ),
                        kind: Some(CodeActionKind::new(DANGEROUS_KIND)),
                        diagnostics: Some(vec![finding.diagnostic.clone()]),
                        edit: Some(edit),
                        is_preferred: Some(false),
                        ..Default::default()
                    }));
                }
            }
        }
        actions
    }
}
