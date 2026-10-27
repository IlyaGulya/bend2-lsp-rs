use super::super::lsp::Backend;
use super::{
    LintResult,
    control::EXPLAIN_COMMAND,
    wire::{Fix, offsets},
};
use crate::workspace::Document;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionDisabled, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionTriggerKind, Command, Diagnostic, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, TextDocumentEdit, TextEdit, WorkspaceEdit,
};

pub(in crate::server) const DANGEROUS_KIND: &str = "quickfix.bend-lint.dangerous";
const EXPLAIN_KIND: &str = "quickfix.bend-lint.explain";
const RECHECK_REASON: &str =
    "This bend-lint result is no longer current. Recheck the file before applying a fix.";

fn requested(params: &CodeActionParams, action_kind: &str) -> bool {
    params.context.trigger_kind == Some(CodeActionTriggerKind::INVOKED)
        && params.context.only.as_ref().is_none_or(|kinds| {
            kinds.iter().any(|kind| {
                let kind = kind.as_str();
                kind.is_empty()
                    || kind == action_kind
                    || action_kind
                        .strip_prefix(kind)
                        .is_some_and(|tail| tail.starts_with('.'))
            })
        })
}

fn versioned_fix(
    backend: &Backend,
    result: &LintResult,
    fix: &Fix,
) -> Result<WorkspaceEdit, &'static str> {
    if fix.applicability != "dangerous" {
        return Err(
            "This bend-lint fix uses an unsupported applicability; no edit can be applied safely.",
        );
    }
    if fix.edits.is_empty() || fix.edits.len() > 4096 {
        return Err("This bend-lint fix has an invalid or unsupported number of edits.");
    }
    let mut edits = BTreeMap::<PathBuf, (Document, Vec<(usize, usize, TextEdit)>)>::new();
    for edit in &fix.edits {
        let (path, checked) = result
            .snapshot
            .source(&edit.path)
            .ok_or("This fix targets a file outside the checked project inputs.")?;
        let uri = result
            .snapshot
            .uris
            .get(&path)
            .ok_or("This fix targets a file without a supported document URI.")?;
        // Closed targets have no document version. Refuse rather than give an
        // unversioned edit that could overwrite a later disk/editor change.
        let document = backend
            .document(uri)
            .ok_or("Open every file targeted by this fix before applying it.")?;
        if document.text != checked.text {
            return Err(
                "The open buffer changed after analysis; recheck before applying this fix.",
            );
        }
        if !backend.diagnostics_snapshot_current(uri, &document.snapshot) {
            return Err("A target document is no longer current. Recheck before applying this fix.");
        }
        let (start, end) = offsets(checked, edit.range)
            .ok_or("This bend-lint fix has an invalid UTF-16 edit range.")?;
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
            return Err("This bend-lint fix has overlapping edits and cannot be applied safely.");
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
    Ok(WorkspaceEdit {
        document_changes: Some(DocumentChanges::Edits(changes)),
        ..Default::default()
    })
}

impl Backend {
    fn unavailable_lint_action(
        &self,
        params: &CodeActionParams,
        title: String,
        diagnostic: Diagnostic,
        reason: &str,
    ) -> Option<CodeActionOrCommand> {
        let mut action = CodeAction {
            title,
            diagnostics: Some(vec![diagnostic]),
            is_preferred: Some(false),
            ..Default::default()
        };
        if self.registration.read().lint.disabled_code_actions && requested(params, DANGEROUS_KIND)
        {
            action.kind = Some(CodeActionKind::new(DANGEROUS_KIND));
            action.disabled = Some(CodeActionDisabled {
                reason: reason.into(),
            });
        } else if requested(params, EXPLAIN_KIND) {
            action.kind = Some(CodeActionKind::new(EXPLAIN_KIND));
            action.title = format!("Why this bend-lint fix is unavailable: {}", action.title);
            action.command = Some(Command {
                title: "Explain unavailable bend-lint fix".into(),
                command: EXPLAIN_COMMAND.into(),
                arguments: Some(vec![serde_json::json!({
                    "uri": params.text_document.uri,
                    "reason": reason,
                })]),
            });
        } else {
            return None;
        }
        Some(CodeActionOrCommand::CodeAction(action))
    }

    pub(in crate::server) async fn lint_code_actions(
        &self,
        params: &CodeActionParams,
    ) -> Vec<CodeActionOrCommand> {
        if !requested(params, DANGEROUS_KIND) && !requested(params, EXPLAIN_KIND) {
            return Vec::new();
        }
        let uri = &params.text_document.uri;
        let Some(result) = self.current_lint_result(uri) else {
            return params
                .context
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.source.as_deref() == Some("bend-lint"))
                .filter_map(|diagnostic| {
                    self.unavailable_lint_action(
                        params,
                        "Recheck bend-lint findings".into(),
                        diagnostic.clone(),
                        RECHECK_REASON,
                    )
                })
                .collect();
        };
        let snapshot = result.snapshot.clone();
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
                snapshot.unchanged()
            })
            .await,
        );
        let current = self
            .current_lint_result(uri)
            .is_some_and(|current| Arc::ptr_eq(&current, &result));
        let mut actions = Vec::new();
        for finding in &result.findings.actionable {
            if finding.uri != *uri
                || finding.diagnostic.range.end < params.range.start
                || params.range.end < finding.diagnostic.range.start
            {
                continue;
            }
            for fix in &finding.fixes {
                let title = format!(
                    "Dangerous bend-lint fix (may change behavior): {}",
                    fix.title
                );
                let edit = if !fresh || !current {
                    Err(RECHECK_REASON)
                } else if !result.config.dangerous_code_actions {
                    Err(
                        "Enable bend.lint.dangerousCodeActions to opt into fixes that may change program behavior.",
                    )
                } else {
                    versioned_fix(self, &result, fix)
                };
                match edit {
                    Ok(edit) if requested(params, DANGEROUS_KIND) => {
                        actions.push(CodeActionOrCommand::CodeAction(CodeAction {
                            title,
                            kind: Some(CodeActionKind::new(DANGEROUS_KIND)),
                            diagnostics: Some(vec![finding.diagnostic.clone()]),
                            edit: Some(edit),
                            is_preferred: Some(false),
                            ..Default::default()
                        }));
                    }
                    Err(reason) => {
                        if let Some(action) = self.unavailable_lint_action(
                            params,
                            title,
                            finding.diagnostic.clone(),
                            reason,
                        ) {
                            actions.push(action);
                        }
                    }
                    Ok(_) => {}
                }
            }
        }
        actions
    }
}
