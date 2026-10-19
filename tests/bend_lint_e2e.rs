#[path = "support/lsp_client.rs"]
pub mod lsp_client;
mod support;

use bend2_lsp::analysis::LineIndex;
use lsp_client::LspClient;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use support::Must;
use tempfile::tempdir;
use tower_lsp::lsp_types::TextEdit;
use url::Url;

const ANALYZER_TIMEOUT_MS: u64 = 120_000;

// This is intentionally opt-in: unlike protocol fixtures it executes real,
// trusted project rules and may populate upstream's pinned Bend/BendHub cache.
// BEND2_LINT_TEST_PATH is an absolute installed bend-lint executable, or Bun.
// For Bun, BEND2_LINT_TEST_ARGUMENTS is a JSON string array containing the
// installed or officially shipped src/linter/seam.ts path, not the raw checkout,
// bunx or a package manager. Shipping preserves the published rule API identity.
// The tested upstream schema is revision
// 02ef84b38b8c06cbd607e270c4930e633e9dab0f, not the older PR1389 CLI.
#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn genuine_analyzer_matches_its_cli_report_through_saved_stdio_document() {
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("real analyzer workspace");
    let project = directory.path().join("genuine analyzer project 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    // Independent expected source: only the four-space body becomes two spaces.
    let source = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n    Nat.add(x, y)\n";
    let canonical = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";
    let root = project.join("main.bend");
    fs::write(&root, source).must_be("write genuine Bend source");
    fs::write(
        project.join("bend-lint.json"),
        "{\"packages\":{\"bend-lint-rules\":\"0.1.0.0\"},\"rules\":{\"bend-lint-rules/language/unneeded_plus\":{},\"bend-lint-rules/layout/format\":{}}}",
    )
    .must_be("enable genuine upstream rules");
    let report = match std::panic::catch_unwind(|| genuine_report(&executable, &arguments, &root)) {
        Ok(report) => report,
        Err(failure) => {
            retain_failed_analyzer(directory, &root, &executable, &arguments);
            std::panic::resume_unwind(failure);
        }
    };
    let expected = finding(
        &report["findings"],
        "bend-lint-rules/language/unneeded_plus",
    );
    assert_eq!(expected["range"]["start"], json!({"line":2,"character":7}));
    assert_eq!(expected["range"]["end"], json!({"line":2,"character":8}));
    let format = finding(&report["findings"], "bend-lint-rules/layout/format");
    assert_eq!(format["range"]["start"], json!({"line":3,"character":2}));
    let reported_edit = reported_format_edit(format, &root, canonical);

    let lsp_started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&root);
    let packet = saved_diagnostics(&mut client, &document, source, lsp_started);
    let diagnostics = &packet["params"]["diagnostics"];
    let actual = finding(diagnostics, "bend-lint-rules/language/unneeded_plus");
    assert_eq!(actual["source"], "bend-lint");
    assert_eq!(actual["message"], expected["message"]);
    assert_eq!(actual["range"], expected["range"]);
    assert_eq!(actual["severity"], 4, "upstream Hint remains LSP Hint");
    let format_actual = finding(diagnostics, "bend-lint-rules/layout/format");
    assert_eq!(format_actual["source"], "bend-lint");
    assert_eq!(format_actual["message"], format["message"]);
    assert_eq!(format_actual["range"], format["range"]);
    assert_eq!(
        format_actual["severity"], 2,
        "upstream Warning remains LSP Warning"
    );
    explicit_user_fix(
        &mut client,
        &root,
        source,
        canonical,
        format_actual,
        reported_edit,
    );
    client.finish();
}

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn organize_imports_satisfies_genuine_analyzer_and_preserves_other_findings() {
    const IMPORTS: &str = "bend-lint-rules/layout/imports";
    const CANONICAL_PATH: &str = "bend-lint-rules/layout/canon_import_path";
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("real import ordering workspace");
    let project = directory.path().join("import ordering 🦀");
    fs::create_dir_all(project.join("support")).must_be("create imported modules");
    for name in ["alpha", "zeta"] {
        fs::write(
            project.join("support").join(format!("{name}.bend")),
            "import Base\n\ndef one() -> U32:\n  1\n",
        )
        .must_be("write real imported module");
    }
    let source = concat!(
        "import support/zeta.bend as zeta\n",
        "import Base\n",
        "import ./support/alpha.bend as Alpha\n",
        "\ndef main() -> U32:\n",
        "  (zeta.one() + Alpha.one() : U32)\n",
    );
    let root = project.join("main.bend");
    fs::write(&root, source).must_be("write unsorted imports");
    fs::write(
        project.join("bend-lint.json"),
        serde_json::to_vec(&json!({
            "packages":{"bend-lint-rules":"0.1.0.0"},
            "rules":{IMPORTS:{},CANONICAL_PATH:{}}
        }))
        .must_be("serialize real import rules"),
    )
    .must_be("enable genuine import rules");
    let before_cli = genuine_report(&executable, &arguments, &root);
    finding(&before_cli["findings"], IMPORTS);
    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let document = uri(&root);
    let before_lsp = saved_diagnostics(&mut client, &document, source, started);
    finding(&before_lsp["params"]["diagnostics"], IMPORTS);
    let organized = organize_imports_source(&mut client, &root, source, 1)
        .must_be("organize imports action for the reported order");
    fs::write(&root, &organized).must_be("user applies the import action");
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument":{"uri":document,"version":2},
            "contentChanges":[{"text":organized}]
        }),
    );
    // This versioned read also acknowledges the editor's preceding mutation.
    assert!(
        organize_imports_source(&mut client, &root, &organized, 2).is_none(),
        "organizing the edited result must be idempotent"
    );
    // Let dependency/index publication finish before this fresh save; workspace
    // changes intentionally retire disk lint jobs while they are in flight.
    client
        .receive_matching(Duration::from_secs(10), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == document
                && message["params"]["version"] == 2
        })
        .must_be("updated document publication before the fresh save");
    client.notify(
        "textDocument/didSave",
        json!({"textDocument":{"uri":document}}),
    );
    // The still-noncanonical path is an independent finding. Its arrival at
    // revision 2 proves lint completed, rather than observing an interim clear.
    let after_lsp = wait_for_saved_diagnostics(&mut client, &document, started, 2);
    let after_cli = genuine_report(&executable, &arguments, &root);
    for findings in [&after_cli["findings"], &after_lsp["params"]["diagnostics"]] {
        assert!(
            findings
                .as_array()
                .must_be("genuine findings after organizing")
                .iter()
                .all(|finding| finding["code"] != IMPORTS),
            "Organize imports must satisfy the genuine import-order rule: {findings}"
        );
        finding(findings, CANONICAL_PATH);
    }
    client.finish();
}

fn retain_failed_analyzer(
    directory: tempfile::TempDir,
    root: &Path,
    executable: &Path,
    arguments: &[String],
) {
    let Some(output) = env::var_os("BEND2_LINT_TEST_FAILURE_DIR").filter(|value| !value.is_empty())
    else {
        return;
    };
    let output = PathBuf::from(output);
    fs::create_dir_all(&output).must_be("create analyzer failure evidence directory");
    let retained = directory.keep();
    let project = root.parent().must_be("retained genuine project");
    for name in ["main.bend", "bend-lint.json"] {
        fs::copy(project.join(name), output.join(name)).must_be("retain genuine fixture input");
    }
    let replay = json!({
        "executable": executable,
        "arguments": arguments,
        "entry": root,
        "cwd": project,
        "retained_root": retained,
    });
    fs::write(
        output.join("replay.json"),
        serde_json::to_vec_pretty(&replay).must_be("serialize exact failed analyzer invocation"),
    )
    .must_be("retain exact failed analyzer invocation");
}

fn configured_analyzer() -> (PathBuf, Vec<String>) {
    let executable = PathBuf::from(
        env::var_os("BEND2_LINT_TEST_PATH")
            .must_be("set BEND2_LINT_TEST_PATH to the genuine installed analyzer or Bun"),
    );
    assert!(
        executable.is_absolute(),
        "use an explicit absolute executable"
    );
    let arguments: Vec<String> = env::var("BEND2_LINT_TEST_ARGUMENTS").map_or_else(
        |_| Vec::new(),
        |value| serde_json::from_str(&value).must_be("JSON analyzer argument array"),
    );
    assert!(
        !arguments
            .iter()
            .any(|argument| argument.starts_with("--fix"))
    );
    (executable, arguments)
}

fn genuine_report(executable: &Path, arguments: &[String], root: &Path) -> Value {
    let started = Instant::now();
    let cli = Command::new(executable)
        .args(arguments)
        .arg(root)
        .arg("--json")
        .args(["--jobs", "1"])
        .current_dir(root.parent().must_be("genuine project"))
        .output()
        .must_be("run genuine bend-lint CLI without fixes");
    eprintln!(
        "genuine analyzer CLI completed after {:.3}s with {}; stderr:\n{}",
        started.elapsed().as_secs_f64(),
        cli.status,
        String::from_utf8_lossy(&cli.stderr)
    );
    assert!(
        cli.status.success(),
        "real analyzer failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report: Value =
        serde_json::from_slice(&cli.stdout).must_be("genuine upstream CLI JSON envelope");
    assert_eq!(report["ok"], true);
    assert!(report["suppressed"].is_array());
    report
}

fn finding<'a>(findings: &'a Value, code: &str) -> &'a Value {
    findings
        .as_array()
        .must_be("genuine findings")
        .iter()
        .find(|finding| finding["code"] == code)
        .must_be("genuine expected rule finding")
}

fn reported_format_edit<'a>(format: &'a Value, root: &Path, canonical: &str) -> &'a Value {
    let fixes = format["fixes"].as_array().must_be("real reported fixes");
    assert_eq!(fixes.len(), 1);
    assert_eq!(fixes[0]["applicability"], "dangerous");
    let edits = fixes[0]["edits"].as_array().must_be("real reported edits");
    assert_eq!(edits.len(), 1);
    let edit = &edits[0];
    let target = Path::new(edit["path"].as_str().must_be("reported edit path"));
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        root.parent().must_be("genuine project").join(target)
    };
    assert_eq!(
        fs::canonicalize(target).must_be("actual reported edit target"),
        fs::canonicalize(root).must_be("actual source path")
    );
    assert_eq!(
        edit["range"],
        json!({
            "start":{"line":0,"character":0},
            "end":{"line":4,"character":0}
        })
    );
    assert_eq!(edit["text"], canonical);
    edit
}

fn configured_client(project: &Path, executable: &Path, arguments: &[String]) -> LspClient {
    let mut command = env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
        || Command::new(env!("CARGO_BIN_EXE_bend2-lsp")),
        Command::new,
    );
    command.current_dir(project).stderr(Stdio::inherit());
    let mut client = LspClient::spawn(command);
    client.initialize(project);
    let mut settings = json!({"bend":{"lint":{
        "enabled":true,"path":executable,"arguments":arguments,
        "dangerousCodeActions":true,"timeoutMs":ANALYZER_TIMEOUT_MS
    }}});
    if let Some(compiler) = env::var_os("BEND2_COMPILER_TEST_PATH") {
        let compiler = PathBuf::from(compiler);
        assert!(compiler.is_absolute());
        settings["bend2-lsp"] = json!({"compilerPath":compiler});
    }
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":settings}),
    );
    client
}

fn saved_diagnostics(
    client: &mut LspClient,
    document: &str,
    source: &str,
    started: Instant,
) -> Value {
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":source
        }}),
    );
    assert!(
        client
            .receive_matching(Duration::from_millis(350), |message| {
                lint_packet(message, document)
            })
            .is_none(),
        "even genuine rules must wait for save"
    );
    eprintln!(
        "[LSP +{:.3}s] didSave; analyzer timeout is {ANALYZER_TIMEOUT_MS}ms",
        started.elapsed().as_secs_f64()
    );
    client.notify(
        "textDocument/didSave",
        json!({"textDocument":{"uri":document}}),
    );
    let packet = wait_for_saved_diagnostics(client, document, started, 1);
    assert_eq!(packet["params"]["version"], 1);
    packet
}

fn wait_for_saved_diagnostics(
    client: &mut LspClient,
    document: &str,
    started: Instant,
    version: i32,
) -> Value {
    let waiting = Instant::now();
    let deadline = waiting + Duration::from_secs(150);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Include already buffered messages, with their original receive times:
        // initialization/configuration warnings can explain a missing provider.
        let Some((message, received)) = client.receive_matching_timed(remaining, |message| {
            message["method"] == "window/logMessage"
                || (message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == document)
        }) else {
            break;
        };
        eprintln!(
            "[LSP +{:.3}s] {message}",
            received.duration_since(started).as_secs_f64()
        );
        if lint_packet(&message, document) && message["params"]["version"] == version {
            return message;
        }
    }
    panic!(
        "real analyzer finding did not arrive through stdio LSP after {:.3}s \
         (configured analyzer timeout: {ANALYZER_TIMEOUT_MS}ms; LSP exit status: {:?}); \
         see timestamped window/logMessage and publishDiagnostics messages above",
        waiting.elapsed().as_secs_f64(),
        client.try_wait()
    );
}

fn explicit_user_fix(
    client: &mut LspClient,
    root: &Path,
    source: &str,
    canonical: &str,
    diagnostic: &Value,
    reported_edit: &Value,
) {
    let document = uri(root);
    let automatic = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},"range":diagnostic["range"],
            "context":{"diagnostics":[diagnostic],"triggerKind":2,
                "only":["quickfix.bend-lint.dangerous"]}
        }),
    );
    assert_eq!(
        automatic["result"],
        json!([]),
        "no automatic dangerous action"
    );
    let response = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},"range":diagnostic["range"],
            "context":{"diagnostics":[diagnostic],"triggerKind":1,
                "only":["quickfix.bend-lint.dangerous"]}
        }),
    );
    let actions = response["result"]
        .as_array()
        .must_be("explicit genuine actions");
    assert_eq!(actions.len(), 1, "only the actual reported dangerous fix");
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    assert_eq!(action["isPreferred"], false);
    assert!(
        action["title"]
            .as_str()
            .must_be("dangerous title")
            .starts_with("Dangerous")
    );
    assert_eq!(action["diagnostics"][0]["code"], diagnostic["code"]);
    assert!(action["command"].is_null(), "only client-applied edits");
    assert!(
        action["edit"]["changes"].is_null(),
        "never unversioned changes"
    );
    let changes = action["edit"]["documentChanges"]
        .as_array()
        .must_be("versioned edit");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["textDocument"]["uri"], document);
    assert_eq!(changes[0]["textDocument"]["version"], 1);
    let edits = changes[0]["edits"].as_array().must_be("actual replacement");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["range"], reported_edit["range"]);
    assert_eq!(edits[0]["newText"], reported_edit["text"]);
    assert_eq!(
        fs::read_to_string(root).must_be("unchanged genuine source"),
        source
    );
    // Simulate a user's acceptance only after validating the full-file range
    // and document version; the server itself must never write the edit.
    let mut applied = source.to_owned();
    let end = applied.len();
    applied.replace_range(
        0..end,
        edits[0]["newText"].as_str().must_be("replacement text"),
    );
    assert_eq!(
        applied, canonical,
        "independently expected two-space formatting"
    );
    fs::write(root, &applied).must_be("user explicitly applies the versioned edit");
    assert_eq!(
        fs::read_to_string(root).must_be("user-applied source"),
        canonical
    );
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument":{"uri":document,"version":2},
            "contentChanges":[{"text":applied}]
        }),
    );
}

fn organize_imports_source(
    client: &mut LspClient,
    root: &Path,
    source: &str,
    version: i32,
) -> Option<String> {
    let document = uri(root);
    let response = client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},
            "context":{"diagnostics":[],"only":["source.organizeImports"],"triggerKind":1}
        }),
    );
    let action = response["result"]
        .as_array()
        .must_be("import action response")
        .iter()
        .find(|action| action["kind"] == "source.organizeImports")?;
    let changes = action["edit"]["documentChanges"]
        .as_array()
        .must_be("versioned import edit");
    assert_eq!(
        changes.len(),
        1,
        "organizing only edits the current document"
    );
    assert_eq!(changes[0]["textDocument"]["uri"], document);
    assert_eq!(changes[0]["textDocument"]["version"], version);
    let edits: Vec<TextEdit> =
        serde_json::from_value(changes[0]["edits"].clone()).must_be("import text edits");
    let index = LineIndex::new(source);
    let mut edits: Vec<_> = edits
        .into_iter()
        .map(|edit| {
            (
                index.offset(source, edit.range.start.line, edit.range.start.character),
                index.offset(source, edit.range.end.line, edit.range.end.character),
                edit.new_text,
            )
        })
        .collect();
    edits.sort_unstable_by_key(|(start, end, _)| (*start, *end));
    let mut result = source.to_owned();
    for (start, end, replacement) in edits.into_iter().rev() {
        result.replace_range(start..end, &replacement);
    }
    Some(result)
}

fn uri(path: &Path) -> String {
    Url::from_file_path(path)
        .must_be("real file URI")
        .to_string()
}

fn lint_packet(message: &Value, document: &str) -> bool {
    message["method"] == "textDocument/publishDiagnostics"
        && message["params"]["uri"] == document
        && message["params"]["diagnostics"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["source"] == "bend-lint"))
}
