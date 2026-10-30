use super::super::lsp::Backend;
use super::{
    LintResult,
    control::EXPLAIN_COMMAND,
    wire::{ActionableFinding, Fix, offsets},
};
use crate::{analysis::DocumentSnapshot, workspace::Document};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};
use tower_lsp::lsp_types::{
    CodeAction, CodeActionDisabled, CodeActionKind, CodeActionOrCommand, CodeActionParams,
    CodeActionTriggerKind, Command, Diagnostic, DocumentChanges, OneOf,
    OptionalVersionedTextDocumentIdentifier, Range, TextDocumentEdit, TextEdit, WorkspaceEdit,
};
use url::Url;

pub(in crate::server) const DANGEROUS_KIND: &str = "quickfix.bend-lint.dangerous";
const EXPLAIN_KIND: &str = "quickfix.bend-lint.explain";
const RECHECK_REASON: &str =
    "This bend-lint result is no longer current. Recheck the file before applying a fix.";

fn requested(params: &CodeActionParams, action_kind: &str) -> bool {
    // CodeActionContext.triggerKind is optional; an omitted kind is an invoked
    // request, while an explicit automatic request never exposes these fixes.
    matches!(
        params.context.trigger_kind,
        None | Some(CodeActionTriggerKind::INVOKED)
    ) && params.context.only.as_ref().is_none_or(|kinds| {
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

// A fix whose physical target identities, current open spellings, and
// versioned edit were resolved by the bounded blocking validation stage.
struct PreparedFix {
    edit: WorkspaceEdit,
    targets: Vec<PreparedTarget>,
}

// The open document a prepared edit is versioned to. Finalization revalidates
// that exact open snapshot in memory under the caller's workspace read view.
struct PreparedTarget {
    uri: Arc<Url>,
    snapshot: Arc<DocumentSnapshot>,
}

struct PreparedAction {
    result: Arc<LintResult>,
    finding: usize,
    fix: usize,
    edit: Result<PreparedFix, &'static str>,
}

// The result of the analyzer-semaphore and filesystem validation stage. It
// carries no workspace lock: the caller ends this stage, then reacquires a
// short read view for pure in-memory finalization.
pub(in crate::server) struct PreparedLintActions {
    uri: Url,
    open_views: BTreeSet<Url>,
    contributors_present: bool,
    actions: Vec<PreparedAction>,
}

fn versioned_fix(
    backend: &Backend,
    open_views: &BTreeSet<Url>,
    result: &LintResult,
    fix: &Fix,
) -> Result<PreparedFix, &'static str> {
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
        let (start, end) = offsets(checked, edit.range)
            .ok_or("This bend-lint fix has an invalid UTF-16 edit range.")?;
        let text_edit = || TextEdit {
            range: edit.range,
            new_text: edit.text.clone(),
        };
        // Validate and resolve each unique target once; repeated edits to one
        // file reuse the already verified document and version.
        if let Some((_, existing)) = edits.get_mut(&path) {
            existing.push((start, end, text_edit()));
            continue;
        }
        // A current physical alias that was never captured must still make the
        // target ambiguous. If the checked target's identity cannot be
        // verified, no alias can be ruled out: fail closed instead of editing
        // whatever spelling happens to be captured.
        let identity = super::snapshot::paths::input_identity(&path).ok_or(
            "This fix target's physical file identity cannot be verified; save it and recheck before applying this fix.",
        )?;
        // Resolve the CURRENT open spellings by physical identity, not by one
        // lexical FileId: a symlinked and a canonical spelling (or an alias
        // opened after capture) are different workspace identities but one
        // physical source. Exactly one live target may be edited. The caller
        // captured this immutable set before any filesystem work, so no
        // workspace/database guard is held across these identity probes.
        let uri = {
            let mut open = open_views.iter().filter(|uri| {
                uri.to_file_path()
                    .ok()
                    .and_then(|candidate| super::snapshot::paths::input_identity(&candidate))
                    .as_ref()
                    == Some(&identity)
            });
            match (open.next(), open.next()) {
                // Closed targets have no document version. Refuse rather than
                // give an unversioned edit that could overwrite a later change.
                (None, _) => {
                    return Err("Open every file targeted by this fix before applying it.");
                }
                (Some(uri), None) => uri,
                (Some(_), Some(_)) => {
                    return Err(
                        "This fix targets a file open under multiple URI spellings; close or unify them before applying it.",
                    );
                }
            }
        };
        let document = backend
            .document(uri)
            .ok_or("Open every file targeted by this fix before applying it.")?;
        if document.text != checked.text {
            return Err(
                "The open buffer changed after analysis; recheck before applying this fix.",
            );
        }
        if !backend.diagnostics_snapshot_current(&document.uri, &document.snapshot) {
            return Err(
                "A target document is no longer current. Recheck before applying this fix.",
            );
        }
        edits.insert(path, (document, vec![(start, end, text_edit())]));
    }
    let mut changes = Vec::with_capacity(edits.len());
    let mut targets = Vec::with_capacity(edits.len());
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
        targets.push(PreparedTarget {
            uri: Arc::clone(&document.uri),
            snapshot: Arc::clone(&document.snapshot),
        });
        changes.push(TextDocumentEdit {
            text_document: OptionalVersionedTextDocumentIdentifier {
                uri: document.uri.as_ref().clone(),
                version: Some(version),
            },
            edits: edits
                .into_iter()
                .map(|(_, _, edit)| OneOf::Left(edit))
                .collect(),
        });
    }
    Ok(PreparedFix {
        edit: WorkspaceEdit {
            document_changes: Some(DocumentChanges::Edits(changes)),
            ..Default::default()
        },
        targets,
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

    // One fix is offered by the authority that produced it: the caller
    // resolves its versioned edit from that entry's snapshot and config.
    fn lint_action_for_fix(
        &self,
        params: &CodeActionParams,
        finding: &ActionableFinding,
        fix: &Fix,
        edit: Result<WorkspaceEdit, &'static str>,
    ) -> Option<CodeActionOrCommand> {
        let title = format!(
            "Dangerous bend-lint fix (may change behavior): {}",
            fix.title
        );
        match edit {
            Ok(edit) if requested(params, DANGEROUS_KIND) => {
                Some(CodeActionOrCommand::CodeAction(CodeAction {
                    title,
                    kind: Some(CodeActionKind::new(DANGEROUS_KIND)),
                    diagnostics: Some(vec![finding.diagnostic.clone()]),
                    edit: Some(edit),
                    is_preferred: Some(false),
                    ..Default::default()
                }))
            }
            Err(reason) => {
                self.unavailable_lint_action(params, title, finding.diagnostic.clone(), reason)
            }
            Ok(_) => None,
        }
    }

    // Immutable contributor selection without filesystem work. Physical input
    // freshness is validated later under the bounded blocking stage.
    fn current_lint_candidates(&self, uri: &Url) -> Vec<Arc<LintResult>> {
        let contributors = self.diagnostics.lint.results.read().contributors(uri);
        if contributors.is_empty() {
            return contributors;
        }
        let projects = self.diagnostics.lint.projects.read();
        let config = self.diagnostics.lint.config.read();
        contributors
            .into_iter()
            .filter(|result| {
                projects
                    .get(&result.project)
                    .is_some_and(|state| state.revision == result.revision)
                    && *config == result.config
                    && self.lint_revisions_current(&result.revisions)
            })
            .collect()
    }

    // Bounded preparation: select immutable inputs, wait for the shared
    // analyzer semaphore, and do every filesystem identity/freshness check on
    // the blocking pool without holding the workspace update view.
    pub(in crate::server) async fn prepare_lint_code_actions(
        &self,
        params: &CodeActionParams,
    ) -> PreparedLintActions {
        let uri = params.text_document.uri.clone();
        if !requested(params, DANGEROUS_KIND) && !requested(params, EXPLAIN_KIND) {
            return PreparedLintActions {
                uri,
                open_views: BTreeSet::new(),
                contributors_present: true,
                actions: Vec::new(),
            };
        }
        let contributors = self.current_lint_candidates(&uri);
        if contributors.is_empty() {
            return PreparedLintActions {
                uri,
                open_views: BTreeSet::new(),
                contributors_present: false,
                actions: Vec::new(),
            };
        }
        let Ok(permit) = self
            .diagnostics
            .lint
            .semaphore
            .clone()
            .acquire_owned()
            .await
        else {
            return PreparedLintActions {
                uri,
                open_views: BTreeSet::new(),
                contributors_present: true,
                actions: Vec::new(),
            };
        };
        let requested_range = params.range;
        let backend = self.clone();
        super::super::state::blocking_result(
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                compile_lint_actions(&backend, uri, contributors, requested_range)
            })
            .await,
        )
    }

    // Pure finalization under the caller's short workspace read view: no
    // semaphore wait, no filesystem work, and no new read guard. Every stamp
    // that can change in memory is revalidated here, at response construction.
    pub(in crate::server) fn finalize_lint_code_actions(
        &self,
        params: &CodeActionParams,
        prepared: PreparedLintActions,
    ) -> Vec<CodeActionOrCommand> {
        if !prepared.contributors_present {
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
        }
        let PreparedLintActions {
            uri,
            open_views,
            actions,
            ..
        } = prepared;
        // No candidate fix (automatic/unrelated kind or an out-of-range
        // request): skip every open-view scan and authority lock.
        if actions.is_empty() {
            return Vec::new();
        }
        // Open-view membership cannot change while the caller holds the update
        // read view, so one in-memory sample is coherent. A new or closed
        // alias since preparation invalidates every prepared spelling.
        let views_current = {
            let state = self.workspace.state.read();
            let mut count = 0usize;
            let mut all_captured = true;
            for uri in state.database.open_view_spellings() {
                count += 1;
                all_captured &= open_views.contains(uri);
            }
            all_captured && count == open_views.len()
        };
        let current = self.diagnostics.lint.results.read().contributors(&uri);
        let projects = self.diagnostics.lint.projects.read();
        let config = self.diagnostics.lint.config.read();
        let mut result_actions: Vec<CodeActionOrCommand> = Vec::new();
        for action in actions {
            let PreparedAction {
                result,
                finding: finding_index,
                fix: fix_index,
                edit,
            } = action;
            let finding = &result.findings.actionable[finding_index];
            let fix = &finding.fixes[fix_index];
            let still_current = views_current
                && current
                    .iter()
                    .any(|candidate| Arc::ptr_eq(candidate, &result))
                && projects
                    .get(&result.project)
                    .is_some_and(|state| state.revision == result.revision)
                && *config == result.config
                && self.lint_revisions_current(&result.revisions);
            let edit = if still_current {
                match edit {
                    Ok(prepared_fix) => {
                        let targets_current = prepared_fix.targets.iter().all(|target| {
                            self.diagnostics_snapshot_current(&target.uri, &target.snapshot)
                        });
                        if targets_current {
                            Ok(prepared_fix.edit)
                        } else {
                            Err(RECHECK_REASON)
                        }
                    }
                    Err(reason) => Err(reason),
                }
            } else {
                Err(RECHECK_REASON)
            };
            if let Some(action) = self.lint_action_for_fix(params, finding, fix, edit)
                && !result_actions.contains(&action)
            {
                result_actions.push(action);
            }
        }
        result_actions
    }
}

fn compile_lint_actions(
    backend: &Backend,
    uri: Url,
    contributors: Vec<Arc<LintResult>>,
    requested_range: Range,
) -> PreparedLintActions {
    let inputs = backend.lint_inputs();
    let open_views: BTreeSet<Url> = {
        let state = backend.workspace.state.read();
        state.database.open_view_spellings().cloned().collect()
    };
    let mut actions = Vec::new();
    for result in contributors {
        let fresh = result.snapshot.unchanged()
            && result.snapshot.open_documents_current(&inputs.documents);
        for (finding_index, finding) in result.findings.actionable.iter().enumerate() {
            if finding.uri != uri
                || finding.diagnostic.range.end < requested_range.start
                || requested_range.end < finding.diagnostic.range.start
            {
                continue;
            }
            for (fix_index, fix) in finding.fixes.iter().enumerate() {
                let edit = if !fresh {
                    Err(RECHECK_REASON)
                } else if !result.config.dangerous_code_actions {
                    Err(
                        "Enable bend.lint.dangerousCodeActions to opt into fixes that may change program behavior.",
                    )
                } else {
                    versioned_fix(backend, &open_views, &result, fix)
                };
                actions.push(PreparedAction {
                    result: Arc::clone(&result),
                    finding: finding_index,
                    fix: fix_index,
                    edit,
                });
            }
        }
    }
    PreparedLintActions {
        uri,
        open_views,
        contributors_present: true,
        actions,
    }
}
