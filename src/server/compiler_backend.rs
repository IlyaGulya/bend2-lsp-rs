use crate::analysis::DocumentSnapshot;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock},
};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range};

const FORMAT_VERSION: u32 = 1;

static REGISTRY: LazyLock<Result<Registry, serde_json::Error>> =
    LazyLock::new(|| serde_json::from_str(include_str!("compiler_backend/registry.json")));

pub(super) fn should_probe_native(version_line: &str) -> bool {
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return false;
    }
    let Some(version) = version_line.strip_prefix("bend ") else {
        return false;
    };
    REGISTRY.as_ref().is_ok_and(|registry| {
        registry.schema == FORMAT_VERSION
            && registry.platform == "darwin"
            && registry.arch == "arm64"
            && registry
                .compilers
                .values()
                .any(|compiler| compiler.version == version)
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompilerIdentity {
    pub(super) version: String,
    pub(super) fingerprint: String,
    pub(super) runtime: RuntimeIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RuntimeIdentity {
    pub(super) bun: String,
    pub(super) revision: String,
}

/// The digest and coordinates always refer to this immutable captured snapshot.
#[derive(Debug, Serialize)]
pub(super) struct BackendSource {
    #[serde(skip)]
    original_path: PathBuf,
    #[serde(rename = "path")]
    staged_path: PathBuf,
    #[serde(skip)]
    snapshot: Arc<DocumentSnapshot>,
    sha256: String,
}

impl BackendSource {
    pub(super) fn new(
        original_path: PathBuf,
        staged_path: PathBuf,
        snapshot: Arc<DocumentSnapshot>,
    ) -> io::Result<Self> {
        validate_path(&original_path)?;
        validate_path(&staged_path)?;
        let sha256 = format!("{:x}", Sha256::digest(snapshot.text.as_bytes()));
        Ok(Self {
            original_path,
            staged_path,
            snapshot,
            sha256,
        })
    }

    pub(super) fn original_path(&self) -> &Path {
        &self.original_path
    }

    pub(super) fn staged_path(&self) -> &Path {
        &self.staged_path
    }

    pub(super) fn snapshot(&self) -> &Arc<DocumentSnapshot> {
        &self.snapshot
    }

    pub(super) fn sha256(&self) -> &str {
        &self.sha256
    }
}

#[derive(Debug, Serialize)]
pub(super) struct Request<'a> {
    format_version: u32,
    #[serde(rename = "request_id")]
    id: &'a str,
    mode: RequestMode,
    root: Option<&'a Path>,
    sources: &'a [BackendSource],
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
enum RequestMode {
    Compatibility,
    Recover,
}

impl<'a> Request<'a> {
    pub(super) fn compatibility(request_id: &'a str) -> io::Result<Self> {
        validate_request_id(request_id)?;
        Ok(Self {
            format_version: FORMAT_VERSION,
            id: request_id,
            mode: RequestMode::Compatibility,
            root: None,
            sources: &[],
        })
    }

    pub(super) fn recover(
        request_id: &'a str,
        root: &'a Path,
        sources: &'a [BackendSource],
    ) -> io::Result<Self> {
        validate_request_id(request_id)?;
        let indexed = index_sources(sources)?;
        validate_path(root)?;
        if !indexed.contains_key(root) {
            return Err(invalid("request root is not a captured staged source"));
        }
        Ok(Self {
            format_version: FORMAT_VERSION,
            id: request_id,
            mode: RequestMode::Recover,
            root: Some(root),
            sources,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceDigest {
    path: PathBuf,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompatibilityEnvelope {
    format_version: u32,
    kind: CompatibilityKind,
    request_id: String,
    status: CompatibilityStatus,
    #[serde(deserialize_with = "required_nullable")]
    compiler: Option<CompilerIdentity>,
    #[serde(deserialize_with = "required_nullable")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CompatibilityKind {
    Compatibility,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CompatibilityStatus {
    Checked,
    BackendFailure,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckEnvelope {
    format_version: u32,
    kind: CheckKind,
    request_id: String,
    status: CheckStatus,
    #[serde(deserialize_with = "required_nullable")]
    compiler: Option<CompilerIdentity>,
    sources: Vec<SourceDigest>,
    diagnostics: Vec<CompilerDiagnostic>,
    coverage: Coverage,
    #[serde(deserialize_with = "required_nullable")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum CheckKind {
    Check,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum CheckStatus {
    Checked,
    SourceError,
    BackendFailure,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompilerDiagnostic {
    file: PathBuf,
    #[serde(deserialize_with = "required_nullable")]
    definition: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    range: Option<CompilerRange>,
    span_origin: SpanOrigin,
    message: String,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum SpanOrigin {
    CompilerError,
    ParserDeclaration,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompilerRange {
    start: CompilerPosition,
    end: CompilerPosition,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompilerPosition {
    line: u32,
    character: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Coverage {
    pub(super) declarations: Vec<String>,
    pub(super) checked: Vec<String>,
    pub(super) failed: Vec<String>,
    pub(super) blocked: Vec<String>,
    pub(super) loading_complete: bool,
}

#[derive(Debug)]
pub(super) struct CheckOutcome {
    pub(super) diagnostics: Vec<(PathBuf, Diagnostic)>,
    pub(super) coverage: Coverage,
    pub(super) incomplete: bool,
    pub(super) backend_failure: Option<String>,
}

// An Option normally accepts a missing field. The protocol requires explicit null.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

pub(super) fn decode_compatibility(
    bytes: &[u8],
    request_id: &str,
    exit_code: Option<i32>,
) -> io::Result<CompilerIdentity> {
    let envelope: CompatibilityEnvelope = serde_json::from_slice(bytes).map_err(invalid)?;
    let CompatibilityKind::Compatibility = envelope.kind;
    validate_header(envelope.format_version, &envelope.request_id, request_id)?;
    match envelope.status {
        CompatibilityStatus::Checked => {
            validate_exit(exit_code, 0)?;
            if envelope.error.is_some() {
                return Err(invalid(
                    "successful compatibility response contains an error",
                ));
            }
            let identity = envelope
                .compiler
                .ok_or_else(|| invalid("successful compatibility response has no identity"))?;
            validate_identity(&identity)?;
            Ok(identity)
        }
        CompatibilityStatus::BackendFailure => {
            validate_exit(exit_code, 78)?;
            if let Some(identity) = &envelope.compiler {
                validate_identity(identity)?;
            }
            Err(invalid(required_error(envelope.error)?))
        }
    }
}

pub(super) fn decode_check(
    bytes: &[u8],
    request_id: &str,
    request_root: &Path,
    identity: &CompilerIdentity,
    sources: &[BackendSource],
    exit_code: Option<i32>,
) -> io::Result<CheckOutcome> {
    let envelope: CheckEnvelope = serde_json::from_slice(bytes).map_err(invalid)?;
    let CheckKind::Check = envelope.kind;
    validate_header(envelope.format_version, &envelope.request_id, request_id)?;
    validate_identity(identity)?;
    let expected_exit = match envelope.status {
        CheckStatus::Checked => 0,
        CheckStatus::SourceError => 1,
        CheckStatus::BackendFailure => 78,
    };
    validate_exit(exit_code, expected_exit)?;
    match &envelope.compiler {
        Some(actual) if actual == identity => {}
        None if envelope.status == CheckStatus::BackendFailure => {}
        _ => {
            return Err(invalid(
                "compiler identity differs from the verified capability",
            ));
        }
    }
    let classification = validate_coverage(&envelope.coverage, &envelope.status)?;
    validate_check_state(&envelope)?;
    let indexed = index_sources(sources)?;
    let captured = validate_captured_sources(&envelope.sources, &indexed)?;
    validate_path(request_root)?;
    if !indexed.contains_key(request_root) {
        return Err(invalid("check root is not a captured staged source"));
    }
    if !captured.contains(request_root) && envelope.status != CheckStatus::BackendFailure {
        return Err(invalid(
            "nonfailure check has no captured root source digest",
        ));
    }
    let mut backend_failure = envelope.error;
    let mut diagnostics = Vec::with_capacity(envelope.diagnostics.len());
    let mut diagnosed = HashSet::new();
    for diagnostic in envelope.diagnostics {
        validate_diagnostic(&diagnostic)?;
        if let Some(definition) = diagnostic.definition {
            if classification.get(definition.as_str()) == Some(&true) {
                return Err(invalid(
                    "a diagnosed declaration is marked successfully checked",
                ));
            }
            diagnosed.insert(definition);
        }
        let Some(source) = indexed.get(diagnostic.file.as_path()) else {
            // Native library files cannot be guessed into a user document.
            backend_failure.get_or_insert_with(|| {
                format!(
                    "compiler diagnostic source was not captured: {}",
                    diagnostic.file.display()
                )
            });
            continue;
        };
        if !captured.contains(diagnostic.file.as_path()) {
            return Err(invalid("diagnostic source has no verified response digest"));
        }
        let range = match diagnostic.range {
            Some(range) => validate_range(source.snapshot(), range)?,
            None => Range::default(),
        };
        diagnostics.push((
            source.original_path().to_owned(),
            Diagnostic {
                range,
                severity: Some(DiagnosticSeverity::ERROR),
                code: Some(NumberOrString::String("compiler".to_owned())),
                source: Some("bend2".to_owned()),
                message: diagnostic.message,
                ..Diagnostic::default()
            },
        ));
    }
    let incomplete = backend_failure.is_some()
        || !envelope.coverage.loading_complete
        || !envelope.coverage.blocked.is_empty()
        || envelope
            .coverage
            .failed
            .iter()
            .any(|definition| !diagnosed.contains(definition));
    Ok(CheckOutcome {
        diagnostics,
        coverage: envelope.coverage,
        incomplete,
        backend_failure,
    })
}

fn validate_check_state(envelope: &CheckEnvelope) -> io::Result<()> {
    match envelope.status {
        CheckStatus::Checked => {
            if envelope.error.is_some()
                || !envelope.diagnostics.is_empty()
                || !envelope.coverage.loading_complete
                || !envelope.coverage.failed.is_empty()
                || !envelope.coverage.blocked.is_empty()
            {
                return Err(invalid(
                    "successful check contains errors or incomplete coverage",
                ));
            }
        }
        CheckStatus::SourceError => {
            if envelope.error.is_some() {
                return Err(invalid("source-error response contains a backend error"));
            }
            if envelope.diagnostics.is_empty()
                && envelope.coverage.blocked.is_empty()
                && envelope.coverage.loading_complete
            {
                return Err(invalid(
                    "source-error response contains no errors or incompleteness",
                ));
            }
        }
        CheckStatus::BackendFailure => {
            if !envelope.error.as_deref().is_some_and(nonempty) {
                return Err(invalid("backend failure has no explanation"));
            }
        }
    }
    Ok(())
}

fn validate_coverage<'a>(
    coverage: &'a Coverage,
    status: &CheckStatus,
) -> io::Result<HashMap<&'a str, bool>> {
    let declarations = unique_names(&coverage.declarations)?;
    let mut classified = HashMap::with_capacity(declarations.len());
    for (names, successfully_checked) in [
        (&coverage.checked, true),
        (&coverage.failed, false),
        (&coverage.blocked, false),
    ] {
        for name in names {
            if !nonempty(name) || !declarations.contains(name.as_str()) {
                return Err(invalid("coverage contains an undeclared or empty name"));
            }
            if classified
                .insert(name.as_str(), successfully_checked)
                .is_some()
            {
                return Err(invalid(
                    "coverage sets contain duplicate or overlapping names",
                ));
            }
        }
    }
    if *status != CheckStatus::BackendFailure && classified.len() != declarations.len() {
        return Err(invalid("coverage does not partition all declarations"));
    }
    Ok(classified)
}

fn unique_names(names: &[String]) -> io::Result<HashSet<&str>> {
    let mut unique = HashSet::with_capacity(names.len());
    for name in names {
        if !nonempty(name) || !unique.insert(name.as_str()) {
            return Err(invalid("declarations contain empty or duplicate names"));
        }
    }
    Ok(unique)
}

fn index_sources(sources: &[BackendSource]) -> io::Result<HashMap<&Path, &BackendSource>> {
    let mut staged = HashMap::with_capacity(sources.len());
    let mut originals = HashSet::with_capacity(sources.len());
    for source in sources {
        if staged.insert(source.staged_path(), source).is_some()
            || !originals.insert(source.original_path())
        {
            return Err(invalid("captured source paths are ambiguous"));
        }
    }
    Ok(staged)
}

fn validate_captured_sources<'a>(
    captured: &'a [SourceDigest],
    indexed: &HashMap<&Path, &BackendSource>,
) -> io::Result<HashSet<&'a Path>> {
    let mut unique = HashSet::with_capacity(captured.len());
    for source in captured {
        validate_path(&source.path)?;
        if !unique.insert(source.path.as_path()) {
            return Err(invalid("response contains duplicate source records"));
        }
        let expected = indexed
            .get(source.path.as_path())
            .ok_or_else(|| invalid("response digest refers to an uncaptured source"))?;
        if !is_sha256(&source.sha256) || source.sha256 != expected.sha256() {
            return Err(invalid(
                "response source digest differs from the captured snapshot",
            ));
        }
    }
    Ok(unique)
}

fn validate_diagnostic(diagnostic: &CompilerDiagnostic) -> io::Result<()> {
    validate_path(&diagnostic.file)?;
    if !nonempty(&diagnostic.message)
        || diagnostic
            .definition
            .as_deref()
            .is_some_and(|name| !nonempty(name))
    {
        return Err(invalid(
            "diagnostic contains an empty message or definition",
        ));
    }
    if diagnostic.range.is_none() != (diagnostic.span_origin == SpanOrigin::Unavailable) {
        return Err(invalid("diagnostic range contradicts its span origin"));
    }
    Ok(())
}

fn validate_range(snapshot: &DocumentSnapshot, range: CompilerRange) -> io::Result<Range> {
    let start = validate_position(snapshot, range.start)?;
    let end = validate_position(snapshot, range.end)?;
    if (start.line, start.character) > (end.line, end.character) {
        return Err(invalid("diagnostic range ends before it starts"));
    }
    Ok(Range::new(start, end))
}

fn validate_position(
    snapshot: &DocumentSnapshot,
    position: CompilerPosition,
) -> io::Result<Position> {
    let offset = snapshot
        .line_index
        .offset(&snapshot.text, position.line, position.character);
    if snapshot.line_index.position(&snapshot.text, offset) != (position.line, position.character) {
        return Err(invalid(
            "diagnostic position is not an indexed UTF-16 boundary",
        ));
    }
    Ok(Position::new(position.line, position.character))
}

fn validate_header(version: u32, actual_id: &str, expected_id: &str) -> io::Result<()> {
    validate_request_id(expected_id)?;
    if version != FORMAT_VERSION || actual_id != expected_id {
        return Err(invalid(
            "compiler response version or request ID differs from the request",
        ));
    }
    Ok(())
}

fn validate_request_id(request_id: &str) -> io::Result<()> {
    if nonempty(request_id) {
        Ok(())
    } else {
        Err(invalid("compiler request ID is empty"))
    }
}

fn validate_exit(actual: Option<i32>, expected: i32) -> io::Result<()> {
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(invalid(
            "compiler process exit contradicts the response status",
        ))
    }
}

fn validate_path(path: &Path) -> io::Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("compiler source path is not UTF-8"))?;
    if !path.is_absolute()
        || text.contains('\0')
        || text
            .split(std::path::MAIN_SEPARATOR)
            .skip(1)
            .any(|part| part.is_empty() || part == "." || part == "..")
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid(
            "compiler source path is not an unambiguous absolute UTF-8 path",
        ));
    }
    Ok(())
}

fn nonempty(text: &str) -> bool {
    !text.trim().is_empty()
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn required_error(error: Option<String>) -> io::Result<String> {
    error
        .filter(|message| nonempty(message))
        .ok_or_else(|| invalid("backend failure has no explanation"))
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    schema: u32,
    platform: String,
    arch: String,
    families: HashMap<String, RuntimeIdentity>,
    compilers: HashMap<String, RegisteredCompiler>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisteredCompiler {
    version: String,
    family: String,
    runtime: Option<RuntimeIdentity>,
}

fn validate_identity(identity: &CompilerIdentity) -> io::Result<()> {
    let registry = REGISTRY.as_ref().map_err(invalid)?;
    if registry.schema != FORMAT_VERSION
        || registry.platform != "darwin"
        || registry.arch != "arm64"
    {
        return Err(invalid("compiler identity registry is incompatible"));
    }
    let compiler = registry
        .compilers
        .get(&identity.fingerprint)
        .filter(|_| is_sha256(&identity.fingerprint))
        .ok_or_else(|| invalid("compiler fingerprint is not verified"))?;
    let runtime = compiler
        .runtime
        .as_ref()
        .or_else(|| registry.families.get(&compiler.family))
        .ok_or_else(|| invalid("compiler registry has no runtime pin"))?;
    if compiler.version != identity.version || *runtime != identity.runtime {
        return Err(invalid(
            "compiler version or embedded runtime differs from its verified pin",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        BackendSource, CheckOutcome, CompilerIdentity, REGISTRY, decode_check,
        decode_compatibility, invalid, validate_path,
    };
    use crate::analysis::{DocumentSnapshot, Revision};
    use serde_json::{Value, json};
    use std::{error::Error, io, path::PathBuf, sync::Arc};
    use tower_lsp::lsp_types::{Position, Range};

    fn identity() -> io::Result<CompilerIdentity> {
        let registry = REGISTRY.as_ref().map_err(invalid)?;
        let (fingerprint, compiler) = registry
            .compilers
            .iter()
            .find(|(_, compiler)| compiler.version == "2.0.36")
            .ok_or_else(|| invalid("missing frozen compiler pin"))?;
        let runtime = compiler
            .runtime
            .as_ref()
            .or_else(|| registry.families.get(&compiler.family))
            .ok_or_else(|| invalid("missing frozen runtime pin"))?;
        Ok(CompilerIdentity {
            version: compiler.version.clone(),
            fingerprint: fingerprint.clone(),
            runtime: runtime.clone(),
        })
    }

    fn fixture_path(path: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!("C:{}", path.replace('/', "\\")))
        } else {
            PathBuf::from(path)
        }
    }

    fn source(text: &str) -> io::Result<BackendSource> {
        BackendSource::new(
            fixture_path("/workspace/main.bend"),
            fixture_path("/staging/main.bend"),
            Arc::new(DocumentSnapshot::new(Revision(4), text.to_owned())),
        )
    }

    fn response(source: &BackendSource, identity: &CompilerIdentity) -> Value {
        json!({
            "format_version": 1, "kind": "check", "request_id": "request-4",
            "status": "source-error", "compiler": identity,
            "sources": [{"path": source.staged_path(), "sha256": source.sha256()}],
            "diagnostics": [{
                "file": source.staged_path(), "definition": "main",
                "range": {"start": {"line": 0, "character": 0},
                          "end": {"line": 0, "character": 2}},
                "span_origin": "compiler-error", "message": "Type mismatch"
            }],
            "coverage": {"declarations": ["main", "independent"],
                         "checked": ["independent"], "failed": ["main"],
                         "blocked": [], "loading_complete": true},
            "error": null
        })
    }

    fn decode(
        value: &Value,
        source: &BackendSource,
        identity: &CompilerIdentity,
    ) -> io::Result<CheckOutcome> {
        decode_check(
            &serde_json::to_vec(value).map_err(invalid)?,
            "request-4",
            source.staged_path(),
            identity,
            std::slice::from_ref(source),
            Some(1),
        )
    }

    #[test]
    fn captured_unicode_ranges_map_only_to_original_document() -> Result<(), Box<dyn Error>> {
        let source = source("😀 value\r\nsecond\n")?;
        let identity = identity()?;
        let outcome = decode(&response(&source, &identity), &source, &identity)?;
        let (path, diagnostic) = outcome
            .diagnostics
            .first()
            .ok_or_else(|| invalid("missing compiler diagnostic"))?;
        assert_eq!(path, source.original_path());
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(0, 0), Position::new(0, 2))
        );
        assert!(!outcome.incomplete);
        assert!(outcome.backend_failure.is_none());
        assert_eq!(outcome.coverage.checked, ["independent"]);
        Ok(())
    }

    #[test]
    fn forged_request_identity_source_and_exit_are_rejected() -> Result<(), Box<dyn Error>> {
        let source = source("😀 value\n")?;
        let identity = identity()?;
        let original = response(&source, &identity);
        for (pointer, replacement) in [
            ("/request_id", json!("superseded-request")),
            ("/format_version", json!(2)),
            ("/kind", json!("compatibility")),
            ("/compiler/fingerprint", json!("0".repeat(64))),
            ("/compiler/runtime/revision", json!("runtime-drift")),
            ("/compiler/version", json!("2.0.35")),
            ("/sources/0/sha256", json!("f".repeat(64))),
            ("/sources/0/path", json!(fixture_path("/another/main.bend"))),
        ] {
            let mut forged = original.clone();
            *forged
                .pointer_mut(pointer)
                .ok_or_else(|| invalid("missing test field"))? = replacement;
            assert!(decode(&forged, &source, &identity).is_err(), "{pointer}");
        }
        let bytes = serde_json::to_vec(&original)?;
        for exit in [None, Some(0), Some(78), Some(2)] {
            assert!(
                decode_check(
                    &bytes,
                    "request-4",
                    source.staged_path(),
                    &identity,
                    std::slice::from_ref(&source),
                    exit
                )
                .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn impossible_utf16_ranges_and_span_claims_are_rejected() -> Result<(), Box<dyn Error>> {
        let source = source("😀 value\r\nnext\n")?;
        let identity = identity()?;
        let original = response(&source, &identity);
        for (start, end) in [
            ((0, 1), (0, 2)), // Inside a surrogate pair.
            ((0, 0), (0, 100)),
            ((9, 0), (9, 0)),
            ((1, 0), (0, 2)),
            ((0, 0), (0, 9)), // Past CRLF-trimmed line content.
        ] {
            let mut forged = original.clone();
            forged["diagnostics"][0]["range"] = json!({
                "start": {"line": start.0, "character": start.1},
                "end": {"line": end.0, "character": end.1}
            });
            assert!(decode(&forged, &source, &identity).is_err());
        }
        let mut unavailable = original;
        unavailable["diagnostics"][0]["range"] = Value::Null;
        assert!(decode(&unavailable, &source, &identity).is_err());
        unavailable["diagnostics"][0]["span_origin"] = json!("unavailable");
        let outcome = decode(&unavailable, &source, &identity)?;
        assert_eq!(outcome.diagnostics[0].1.range, Range::default());
        Ok(())
    }

    #[test]
    fn coverage_cannot_forge_success_or_hide_failed_declarations() -> Result<(), Box<dyn Error>> {
        let source = source("😀 value\n")?;
        let identity = identity()?;
        let original = response(&source, &identity);
        for (pointer, replacement) in [
            (
                "/coverage/declarations",
                json!(["main", "main", "independent"]),
            ),
            ("/coverage/checked", json!(["independent", "independent"])),
            ("/coverage/blocked", json!(["main"])),
            ("/coverage/failed", json!([])),
            ("/coverage/failed", json!(["unknown"])),
            ("/coverage/checked", json!(["main", "independent"])),
        ] {
            let mut forged = original.clone();
            *forged
                .pointer_mut(pointer)
                .ok_or_else(|| invalid("missing test field"))? = replacement;
            assert!(decode(&forged, &source, &identity).is_err(), "{pointer}");
        }
        let mut incomplete = original;
        incomplete["diagnostics"] = json!([]);
        incomplete["coverage"]["loading_complete"] = json!(false);
        let outcome = decode(&incomplete, &source, &identity)?;
        assert!(outcome.incomplete);
        assert!(outcome.diagnostics.is_empty());
        incomplete["coverage"]["loading_complete"] = json!(true);
        assert!(decode(&incomplete, &source, &identity).is_err());
        incomplete["status"] = json!("checked");
        assert!(
            decode_check(
                &serde_json::to_vec(&incomplete)?,
                "request-4",
                source.staged_path(),
                &identity,
                std::slice::from_ref(&source),
                Some(0)
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn unknown_source_and_backend_failure_preserve_independent_errors() -> Result<(), Box<dyn Error>>
    {
        let source = source("😀 value\n")?;
        let identity = identity()?;
        let mut value = response(&source, &identity);
        let mut unknown = value["diagnostics"][0].clone();
        unknown["file"] = json!(fixture_path("/native/library.bend"));
        unknown["definition"] = Value::Null;
        value["diagnostics"]
            .as_array_mut()
            .ok_or_else(|| invalid("missing diagnostics"))?
            .push(unknown);
        let outcome = decode(&value, &source, &identity)?;
        assert_eq!(outcome.diagnostics.len(), 1);
        assert!(outcome.backend_failure.is_some());
        value["status"] = json!("backend-failure");
        value["error"] = json!("Immutable replay failed");
        value["coverage"]["checked"] = json!([]);
        let outcome = decode_check(
            &serde_json::to_vec(&value)?,
            "request-4",
            source.staged_path(),
            &identity,
            std::slice::from_ref(&source),
            Some(78),
        )?;
        assert_eq!(outcome.diagnostics.len(), 1);
        assert_eq!(
            outcome.backend_failure.as_deref(),
            Some("Immutable replay failed")
        );
        assert!(outcome.incomplete);
        Ok(())
    }

    #[test]
    fn duplicate_missing_and_trailing_protocol_fields_are_rejected() -> Result<(), Box<dyn Error>> {
        let source = source("😀 value\n")?;
        let identity = identity()?;
        let value = response(&source, &identity);
        let text = serde_json::to_string(&value)?;
        let duplicate = text.replacen(
            "\"request_id\":\"request-4\"",
            "\"request_id\":\"request-4\",\"request_id\":\"request-4\"",
            1,
        );
        let nested_duplicate = text.replacen("\"line\":0", "\"line\":0,\"line\":0", 1);
        for forged in [
            duplicate,
            nested_duplicate,
            format!("{text} {{}}"),
            format!("{text} garbage"),
        ] {
            assert!(
                decode_check(
                    forged.as_bytes(),
                    "request-4",
                    source.staged_path(),
                    &identity,
                    std::slice::from_ref(&source),
                    Some(1)
                )
                .is_err()
            );
        }
        let mut missing = value;
        missing
            .as_object_mut()
            .ok_or_else(|| invalid("missing envelope"))?
            .remove("error");
        assert!(decode(&missing, &source, &identity).is_err());
        Ok(())
    }

    #[test]
    fn duplicate_and_aliased_source_records_cannot_change_provenance() -> Result<(), Box<dyn Error>>
    {
        let root = source("😀 value\n")?;
        let identity = identity()?;
        let original = response(&root, &identity);
        let mut duplicate = original.clone();
        duplicate["sources"] = json!([original["sources"][0], original["sources"][0]]);
        assert!(decode(&duplicate, &root, &identity).is_err());
        for path in [
            "/staging/./main.bend",
            "/staging//main.bend",
            "/staging/../staging/main.bend",
        ] {
            let path = fixture_path(path);
            assert!(path.is_absolute());
            assert!(validate_path(&path).is_err());
            let mut aliased = original.clone();
            aliased["sources"][0]["path"] = json!(path);
            assert!(decode(&aliased, &root, &identity).is_err());
        }
        let ambiguous = BackendSource::new(
            root.original_path().to_owned(),
            fixture_path("/staging/second.bend"),
            root.snapshot().clone(),
        )?;
        let sources = [root, ambiguous];
        assert!(
            decode_check(
                &serde_json::to_vec(&original)?,
                "request-4",
                sources[0].staged_path(),
                &identity,
                &sources,
                Some(1),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn dependency_digest_cannot_certify_missing_root_or_diagnostic_sources()
    -> Result<(), Box<dyn Error>> {
        let root = source("😀 value\n")?;
        let dependency = BackendSource::new(
            fixture_path("/workspace/dep.bend"),
            fixture_path("/staging/dep.bend"),
            Arc::new(DocumentSnapshot::new(
                Revision(1),
                "def dep = 0\n".to_owned(),
            )),
        )?;
        let identity = identity()?;
        let mut value = response(&root, &identity);
        value["status"] = json!("checked");
        value["diagnostics"] = json!([]);
        value["coverage"]["checked"] = json!(["main", "independent"]);
        value["coverage"]["failed"] = json!([]);
        value["sources"] =
            json!([{"path": dependency.staged_path(), "sha256": dependency.sha256()}]);
        let sources = [root, dependency];
        assert!(
            decode_check(
                &serde_json::to_vec(&value)?,
                "request-4",
                sources[0].staged_path(),
                &identity,
                &sources,
                Some(0),
            )
            .is_err()
        );
        value["status"] = json!("backend-failure");
        value["error"] = json!("Root loading failed");
        let outcome = decode_check(
            &serde_json::to_vec(&value)?,
            "request-4",
            sources[0].staged_path(),
            &identity,
            &sources,
            Some(78),
        )?;
        assert!(outcome.backend_failure.is_some());
        assert!(outcome.incomplete);
        let mut missing_digest = response(&sources[0], &identity);
        missing_digest["diagnostics"][0]["file"] = json!(sources[1].staged_path());
        assert!(
            decode_check(
                &serde_json::to_vec(&missing_digest)?,
                "request-4",
                sources[0].staged_path(),
                &identity,
                &sources,
                Some(1),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn compatibility_requires_the_frozen_identity_and_matching_exit() -> Result<(), Box<dyn Error>>
    {
        let identity = identity()?;
        let original = json!({
            "format_version": 1, "kind": "compatibility", "request_id": "probe-7",
            "status": "checked", "compiler": identity, "error": null
        });
        assert_eq!(
            decode_compatibility(&serde_json::to_vec(&original)?, "probe-7", Some(0))?,
            identity
        );
        for (pointer, replacement) in [
            ("/compiler/runtime/bun", json!("1.4.0")),
            ("/compiler/runtime/revision", json!("drift")),
            ("/compiler/fingerprint", json!("0".repeat(64))),
            ("/request_id", json!("other-probe")),
            ("/compiler", Value::Null),
        ] {
            let mut forged = original.clone();
            *forged
                .pointer_mut(pointer)
                .ok_or_else(|| invalid("missing test field"))? = replacement;
            assert!(
                decode_compatibility(&serde_json::to_vec(&forged)?, "probe-7", Some(0)).is_err()
            );
        }
        assert!(
            decode_compatibility(&serde_json::to_vec(&original)?, "probe-7", Some(78)).is_err()
        );
        Ok(())
    }
}
