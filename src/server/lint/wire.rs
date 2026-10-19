use super::saved::SavedProject;
use crate::analysis::DocumentSnapshot;
use serde::Deserialize;
use std::{collections::HashMap, io};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};
use url::Url;

#[derive(Deserialize)]
struct Report {
    ok: bool,
    findings: Vec<Finding>,
    // Required: the older PR #1389 CLI is a different unsupported protocol.
    suppressed: Vec<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct Finding {
    code: String,
    severity: String,
    message: String,
    path: String,
    range: Range,
    fixes: Vec<Fix>,
}

#[derive(Clone, Deserialize)]
pub(super) struct Fix {
    pub(super) title: String,
    pub(super) applicability: String,
    pub(super) edits: Vec<Edit>,
}

#[derive(Clone, Deserialize)]
pub(super) struct Edit {
    pub(super) path: String,
    pub(super) range: Range,
    pub(super) text: String,
}

pub(super) struct ActionableFinding {
    pub(super) uri: Url,
    pub(super) diagnostic: Diagnostic,
    pub(super) fixes: Vec<Fix>,
}

pub(super) struct Findings {
    pub(super) by_uri: HashMap<Url, Vec<Diagnostic>>,
    pub(super) actionable: Vec<ActionableFinding>,
}

fn severity(value: &str) -> io::Result<DiagnosticSeverity> {
    match value {
        "error" => Ok(DiagnosticSeverity::ERROR),
        "warning" => Ok(DiagnosticSeverity::WARNING),
        "information" => Ok(DiagnosticSeverity::INFORMATION),
        "hint" => Ok(DiagnosticSeverity::HINT),
        _ => Err(io::Error::other("bend-lint returned an unknown severity")),
    }
}

fn position_offset(snapshot: &DocumentSnapshot, position: Position) -> Option<usize> {
    let offset = snapshot
        .line_index
        .offset(&snapshot.text, position.line, position.character);
    (snapshot.line_index.position(&snapshot.text, offset) == (position.line, position.character))
        .then_some(offset)
}

pub(super) fn offsets(snapshot: &DocumentSnapshot, range: Range) -> Option<(usize, usize)> {
    let start = position_offset(snapshot, range.start)?;
    let end = position_offset(snapshot, range.end)?;
    (start <= end).then_some((start, end))
}

fn finding_target(saved: &SavedProject, finding: &Finding) -> io::Result<Url> {
    let (path, snapshot) = saved
        .source(&finding.path)
        .ok_or_else(|| io::Error::other("bend-lint finding path is not a captured source file"))?;
    if offsets(snapshot, finding.range).is_none() {
        return Err(io::Error::other(
            "bend-lint finding has an invalid UTF-16 range",
        ));
    }
    saved.uris.get(&path).cloned().map_or_else(
        || {
            Url::from_file_path(&path)
                .map_err(|()| io::Error::other("bend-lint finding path is not a file URI"))
        },
        Ok,
    )
}

pub(super) fn parse(output: &std::process::Output, saved: &SavedProject) -> io::Result<Findings> {
    let report: Report = serde_json::from_slice(&output.stdout)
        .map_err(|error| io::Error::other(format!("bend-lint malformed JSON: {error}")))?;
    let valid_exit = matches!(
        (output.status.code(), report.ok),
        (Some(0), true) | (Some(1), false)
    );
    if !valid_exit {
        return Err(io::Error::other(format!(
            "bend-lint failed or returned an inconsistent exit status: {}",
            output.status
        )));
    }
    if report.findings.len() > 4096 || report.suppressed.len() > 4096 {
        return Err(io::Error::other(
            "bend-lint returned more than 4096 findings",
        ));
    }
    let mut by_uri = HashMap::<Url, Vec<Diagnostic>>::new();
    let mut actionable = Vec::new();
    let mut fix_count = 0usize;
    let mut edit_count = 0usize;
    for finding in report.findings {
        fix_count = fix_count.saturating_add(finding.fixes.len());
        for fix in &finding.fixes {
            edit_count = edit_count.saturating_add(fix.edits.len());
        }
        if fix_count > 4096 || edit_count > 4096 {
            return Err(io::Error::other(
                "bend-lint returned more than 4096 fixes or edits",
            ));
        }
        let uri = finding_target(saved, &finding)?;
        let diagnostic = Diagnostic {
            range: finding.range,
            severity: Some(severity(&finding.severity)?),
            code: Some(NumberOrString::String(finding.code)),
            source: Some("bend-lint".into()),
            message: finding.message,
            data: Some(
                serde_json::json!({"path":finding.path,"provider":"bend-lint","compiler":"external bend-lint compiler, not the selected LSP Bend"}),
            ),
            ..Diagnostic::default()
        };
        by_uri
            .entry(uri.clone())
            .or_default()
            .push(diagnostic.clone());
        actionable.push(ActionableFinding {
            uri,
            diagnostic,
            fixes: finding.fixes,
        });
    }
    Ok(Findings { by_uri, actionable })
}
