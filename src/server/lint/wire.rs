use super::snapshot::ProjectSnapshot;
use crate::analysis::DocumentSnapshot;
use serde::Deserialize;
use std::{collections::HashMap, io};
use tower_lsp::lsp_types::{
    CodeDescription, Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range,
};
use url::Url;

const MAX_ENTRIES: usize = 4096;
const DETAIL_LIMIT: usize = 16 * 1024;
const NOTICE_LIMIT: usize = 4 * 1024;

#[derive(Deserialize)]
struct Report {
    ok: bool,
    #[serde(default)]
    findings: Option<Vec<Finding>>,
    #[serde(default)]
    suppressed: Option<Vec<serde::de::IgnoredAny>>,
    #[serde(default)]
    skipped: Option<Vec<Skipped>>,
    #[serde(default)]
    errors: Option<Vec<ReportError>>,
}

// A fatal 0.2.1 JSON response: what the analyzer rejected, about which file if
// one. It carries no ranges and must never be turned into diagnostics.
#[derive(Deserialize)]
struct ReportError {
    #[serde(default)]
    path: Option<String>,
    message: String,
}

#[derive(Deserialize)]
struct Finding {
    code: String,
    severity: String,
    message: String,
    path: String,
    range: Range,
    #[serde(default)]
    docs: Option<String>,
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

// Why a rule did not run on a file. Upstream emits no range or code for these,
// so they stay structured operational metadata rather than diagnostics.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum SkippedReason {
    Unloaded,
    Unchecked,
    Unentered,
}

#[derive(Clone, Deserialize)]
pub(super) struct Skipped {
    pub(super) path: String,
    pub(super) reason: SkippedReason,
    pub(super) message: String,
    #[serde(default)]
    pub(super) rules: Vec<String>,
    #[serde(default)]
    pub(super) entry: Option<String>,
}

impl Skipped {
    fn validate(&self) -> io::Result<()> {
        let invalid = |what: &str| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bend-lint skipped entry {what} does not match its reason"),
            )
        };
        if self.path.is_empty() || self.message.is_empty() {
            return Err(invalid("without a path or message"));
        }
        if self.rules.len() > MAX_ENTRIES {
            return Err(invalid("with an excessive number of rules"));
        }
        match self.reason {
            SkippedReason::Unloaded if !self.rules.is_empty() || self.entry.is_some() => {
                Err(invalid("unloaded"))
            }
            SkippedReason::Unchecked if self.rules.is_empty() || self.entry.is_some() => {
                Err(invalid("unchecked"))
            }
            SkippedReason::Unentered
                if self.rules.is_empty() || self.entry.as_deref().is_none_or(str::is_empty) =>
            {
                Err(invalid("unentered"))
            }
            _ => Ok(()),
        }
    }
}

pub(super) struct ActionableFinding {
    pub(super) uri: Url,
    pub(super) diagnostic: Diagnostic,
    pub(super) fixes: Vec<Fix>,
}

pub(super) struct Findings {
    pub(super) by_uri: HashMap<Url, Vec<Diagnostic>>,
    pub(super) actionable: Vec<ActionableFinding>,
    pub(super) skipped: Vec<Skipped>,
}

impl SkippedReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unloaded => "unloaded",
            Self::Unchecked => "unchecked",
            Self::Unentered => "unentered",
        }
    }
}

impl Findings {
    // A bounded operational summary for a partial check. It deliberately says
    // "partially checked": skipped rules or inputs must not look like a clean,
    // complete result, and skips carry no diagnostic location to publish.
    pub(super) fn notice(&self) -> Option<String> {
        if self.skipped.is_empty() {
            return None;
        }
        let mut notice = format!(
            "bend-lint partially checked {} input(s):",
            self.skipped.len()
        );
        for skipped in &self.skipped {
            notice.push(' ');
            notice.push_str(&skipped.path);
            notice.push(' ');
            notice.push_str(skipped.reason.as_str());
            if let Some(entry) = &skipped.entry {
                notice.push_str(" entry ");
                notice.push_str(entry);
            }
            if !skipped.rules.is_empty() {
                notice.push_str(" [");
                notice.push_str(&skipped.rules.join(", "));
                notice.push(']');
            }
            notice.push_str(" (");
            notice.push_str(&skipped.message);
            notice.push_str(");");
            if notice.len() >= NOTICE_LIMIT {
                break;
            }
        }
        Some(bounded(
            notice.replace('\n', " ").replace('\r', " "),
            NOTICE_LIMIT,
        ))
    }
}

fn severity(value: &str) -> io::Result<DiagnosticSeverity> {
    match value {
        "error" => Ok(DiagnosticSeverity::ERROR),
        "warning" => Ok(DiagnosticSeverity::WARNING),
        "information" => Ok(DiagnosticSeverity::INFORMATION),
        "hint" => Ok(DiagnosticSeverity::HINT),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint returned an unknown severity",
        )),
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

fn finding_target(snapshot: &ProjectSnapshot, finding: &Finding) -> io::Result<Url> {
    let (path, source) = snapshot.source(&finding.path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint finding path is not a captured source file",
        )
    })?;
    if offsets(source, finding.range).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint finding has an invalid UTF-16 range",
        ));
    }
    snapshot.uris.get(&path).cloned().map_or_else(
        || {
            Url::from_file_path(&path).map_err(|()| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "bend-lint finding path is not a file URI",
                )
            })
        },
        Ok,
    )
}

fn bounded(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit.saturating_sub('…'.len_utf8());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

fn decode_errors(bytes: &[u8]) -> Option<Vec<ReportError>> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if value.get("ok") != Some(&serde_json::Value::Bool(false)) {
        return None;
    }
    let errors = value.get("errors")?.as_array()?;
    if errors.is_empty() {
        return None;
    }
    errors
        .iter()
        .map(|error| {
            Some(ReportError {
                path: error
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                message: error
                    .get("message")
                    .and_then(serde_json::Value::as_str)?
                    .to_owned(),
            })
        })
        .collect()
}

fn error_detail(errors: &[ReportError]) -> String {
    let mut detail = String::new();
    for error in errors {
        if !detail.is_empty() {
            detail.push_str("; ");
        }
        if let Some(path) = &error.path {
            detail.push_str(path);
            detail.push_str(": ");
        }
        detail.push_str(&error.message);
        if detail.len() >= DETAIL_LIMIT {
            break;
        }
    }
    bounded(detail, DETAIL_LIMIT)
}

// The bounded, attributable text of a fatal JSON response, if stdout is one.
// Shared with preparation failure reporting.
pub(super) fn report_error_detail(stdout: &[u8]) -> Option<String> {
    decode_errors(stdout).map(|errors| error_detail(&errors))
}

fn decode_report(output: &std::process::Output) -> io::Result<Report> {
    serde_json::from_slice(&output.stdout).map_err(|error| {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.trim().is_empty() {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bend-lint malformed JSON: {error}"),
            )
        } else {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "bend-lint malformed JSON: {error}; stderr: {}",
                    bounded(stderr.into_owned(), DETAIL_LIMIT)
                ),
            )
        }
    })
}

fn accepted_report(
    output: &std::process::Output,
    report: Report,
) -> io::Result<(Vec<Finding>, Vec<Skipped>)> {
    match (output.status.code(), report.ok) {
        (Some(2), false) => {
            let detail = report
                .errors
                .as_deref()
                .filter(|errors| !errors.is_empty())
                .map(error_detail)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "bend-lint rejected the project without a structured errors report",
                    )
                })?;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bend-lint rejected the project: {detail}"),
            ));
        }
        (Some(0), true) | (Some(1), false) => {}
        _ => {
            let status = &output.status;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("bend-lint failed or returned an inconsistent exit status: {status}"),
            ));
        }
    }
    if report.errors.as_ref().is_some_and(|errors| !errors.is_empty()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint reported structured errors with a successful exit status",
        ));
    }
    let findings = report.findings.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint 0.2.1 response omits its findings array",
        )
    })?;
    let suppressed = report.suppressed.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint 0.2.1 response omits its suppressed array",
        )
    })?;
    let skipped = report.skipped.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint 0.2.1 response omits its skipped array",
        )
    })?;
    if findings.len() > MAX_ENTRIES || suppressed.len() > MAX_ENTRIES || skipped.len() > MAX_ENTRIES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint returned more than 4096 findings, suppressed entries or skips",
        ));
    }
    for entry in &skipped {
        entry.validate()?;
    }
    Ok((findings, skipped))
}

fn finding_diagnostic(
    snapshot: &ProjectSnapshot,
    finding: Finding,
) -> io::Result<(Url, Diagnostic, Vec<Fix>)> {
    let uri = finding_target(snapshot, &finding)?;
    let code_description = finding
        .docs
        .as_deref()
        .map(|docs| {
            Url::parse(docs)
                .map(|href| CodeDescription { href })
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("bend-lint finding documentation is not a URL: {error}"),
                    )
                })
        })
        .transpose()?;
    let diagnostic = Diagnostic {
        range: finding.range,
        severity: Some(severity(&finding.severity)?),
        code: Some(NumberOrString::String(finding.code)),
        code_description,
        source: Some("bend-lint".into()),
        message: finding.message,
        data: Some(
            serde_json::json!({"path":finding.path,"provider":"bend-lint","compiler":"external bend-lint compiler, not the selected LSP Bend"}),
        ),
        ..Diagnostic::default()
    };
    Ok((uri, diagnostic, finding.fixes))
}

pub(super) fn parse(
    output: &std::process::Output,
    snapshot: &ProjectSnapshot,
) -> io::Result<Findings> {
    let report = decode_report(output)?;
    let (findings, skipped) = accepted_report(output, report)?;
    let mut by_uri = HashMap::<Url, Vec<Diagnostic>>::new();
    let mut actionable = Vec::new();
    let mut fix_count = 0usize;
    let mut edit_count = 0usize;
    for finding in findings {
        fix_count = fix_count.saturating_add(finding.fixes.len());
        for fix in &finding.fixes {
            edit_count = edit_count.saturating_add(fix.edits.len());
        }
        if fix_count > MAX_ENTRIES || edit_count > MAX_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bend-lint returned more than 4096 fixes or edits",
            ));
        }
        let (uri, diagnostic, fixes) = finding_diagnostic(snapshot, finding)?;
        by_uri
            .entry(uri.clone())
            .or_default()
            .push(diagnostic.clone());
        actionable.push(ActionableFinding {
            uri,
            diagnostic,
            fixes,
        });
    }
    Ok(Findings {
        by_uri,
        actionable,
        skipped,
    })
}
