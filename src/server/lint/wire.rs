use super::{snapshot::ProjectSnapshot, util::bounded};
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
            // Upstream reports an entry failure even when no rule requires
            // every entry's findings, so its rules list is legitimately empty.
            SkippedReason::Unentered if self.entry.as_deref().is_none_or(str::is_empty) => {
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
        Some(bounded(notice.replace(['\n', '\r'], " "), NOTICE_LIMIT))
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

// bend-lint 0.2.1 places offsets with Text.Walk, which starts a new line only
// at `\n`; a `\r` before it therefore counts as one UTF-16 line unit. LSP CRLF
// line content excludes that `\r`, so its line length is one unit shorter. The
// only representable equivalent of the upstream position at the LF is the
// line's content end; every other unmatched position stays invalid. Edits keep
// the strict rule below because moving an edit boundary off the CR would change
// what bytes the user applies. (Minimal upstream fix: do not add a column for
// `\r` when the next character is `\n`.)
fn diagnostic_offset(snapshot: &DocumentSnapshot, position: Position) -> Option<usize> {
    if let Some(offset) = position_offset(snapshot, position) {
        return Some(offset);
    }
    let line = usize::try_from(position.line).ok()?;
    let content = snapshot.line_index.line_range(&snapshot.text, line)?;
    let (_, end_character) = snapshot.line_index.position(&snapshot.text, content.end);
    let crlf = snapshot
        .text
        .as_bytes()
        .get(content.end..)
        .is_some_and(|rest| rest.starts_with(b"\r\n"));
    (crlf && position.character == end_character.checked_add(1)?).then_some(content.end)
}

pub(super) fn offsets(snapshot: &DocumentSnapshot, range: Range) -> Option<(usize, usize)> {
    let start = position_offset(snapshot, range.start)?;
    let end = position_offset(snapshot, range.end)?;
    (start <= end).then_some((start, end))
}

// A published diagnostic must carry LSP-valid positions even when bend-lint
// measured a CRLF boundary with its own line-unit rule. Fix edits keep the
// strict offsets above so applying a fix never changes the bytes a client is
// told to replace.
pub(super) fn normalized_range(snapshot: &DocumentSnapshot, range: &Range) -> Option<Range> {
    if range.start > range.end {
        return None;
    }
    let start = diagnostic_offset(snapshot, range.start)?;
    let end = diagnostic_offset(snapshot, range.end)?;
    if start > end {
        return None;
    }
    let (start_line, start_character) = snapshot.line_index.position(&snapshot.text, start);
    let (end_line, end_character) = snapshot.line_index.position(&snapshot.text, end);
    Some(Range {
        start: Position {
            line: start_line,
            character: start_character,
        },
        end: Position {
            line: end_line,
            character: end_character,
        },
    })
}

fn finding_targets(snapshot: &ProjectSnapshot, finding: &Finding) -> io::Result<(Vec<Url>, Range)> {
    let (path, source) = snapshot.source(&finding.path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint finding path is not a captured source file",
        )
    })?;
    let range = normalized_range(source, &finding.range).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint finding has an invalid UTF-16 range",
        )
    })?;
    let uris = snapshot.uris.get(&path).cloned().unwrap_or_default();
    if !uris.is_empty() {
        return Ok((uris, range));
    }
    let uri = Url::from_file_path(&path).map_err(|()| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "bend-lint finding path is not a file URI",
        )
    })?;
    Ok((vec![uri], range))
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
    if report
        .errors
        .as_ref()
        .is_some_and(|errors| !errors.is_empty())
    {
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
) -> io::Result<(Vec<Url>, Diagnostic, Vec<Fix>)> {
    let (uris, range) = finding_targets(snapshot, &finding)?;
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
        range,
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
    Ok((uris, diagnostic, finding.fixes))
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
        let (uris, diagnostic, fixes) = finding_diagnostic(snapshot, finding)?;
        // One packet per current editor spelling; each publication reads that
        // view's own document version.
        for uri in uris {
            by_uri
                .entry(uri.clone())
                .or_default()
                .push(diagnostic.clone());
            actionable.push(ActionableFinding {
                uri,
                diagnostic: diagnostic.clone(),
                fixes: fixes.clone(),
            });
        }
    }
    Ok(Findings {
        by_uri,
        actionable,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::{
        fs,
        path::{Path, PathBuf},
        process::{ExitStatus, Output},
    };

    fn project(text: &str) -> (tempfile::TempDir, ProjectSnapshot, PathBuf) {
        let directory = tempfile::tempdir().expect("range workspace");
        let path = directory.path().join("main.bend");
        fs::write(&path, text).expect("write range source");
        fs::write(directory.path().join("bend-lint.json"), "{\"rules\":{}}")
            .expect("write project configuration");
        let snapshot = ProjectSnapshot::capture(&path, &[]).expect("capture range source");
        (directory, snapshot, path)
    }

    fn report(path: &Path, range: Range) -> Output {
        Output {
            status: ExitStatus::from_raw(0),
            stdout: serde_json::to_vec(&serde_json::json!({
                "ok":true, "suppressed":[], "skipped":[],
                "findings":[{
                    "code":"test/range", "severity":"warning", "message":"Range boundary",
                    "path":path, "range":range, "fixes":[]
                }]
            }))
            .expect("serialize range report"),
            stderr: Vec::new(),
        }
    }

    #[test]
    fn crlf_diagnostics_are_projected_but_edits_keep_exact_boundaries() {
        let (_directory, snapshot, path) = project("  \"😀\"++\"x\"\r\n");
        let upstream = Range::new(Position::new(0, 6), Position::new(0, 12));
        let findings = parse(&report(&path, upstream), &snapshot)
            .expect("the raw CRLF source position has an LSP display equivalent");
        let diagnostic = findings
            .by_uri
            .values()
            .flatten()
            .next()
            .expect("range diagnostic");
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 6), Position::new(0, 11))
        );
        let (_, source) = snapshot
            .source(path.to_str().expect("UTF-8 path"))
            .expect("source");
        assert_eq!(
            offsets(source, upstream),
            None,
            "an edit must not move off the CR"
        );
    }

    #[test]
    fn malformed_ranges_remain_rejected_after_crlf_projection() {
        let (_directory, snapshot, path) = project("  \"😀\"++\"x\"\r\n");
        for range in [
            Range::new(Position::new(0, 6), Position::new(0, 13)),
            Range::new(Position::new(0, 4), Position::new(0, 8)),
            Range::new(Position::new(0, 12), Position::new(0, 11)),
        ] {
            assert!(
                parse(&report(&path, range), &snapshot).is_err(),
                "{range:?}"
            );
        }
        let (_directory, snapshot, path) = project("  \"😀\"++\"x\"\n");
        let past_end = Range::new(Position::new(0, 6), Position::new(0, 12));
        assert!(parse(&report(&path, past_end), &snapshot).is_err());
    }
}
