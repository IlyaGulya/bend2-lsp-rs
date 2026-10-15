#[path = "support/lsp_client.rs"]
pub mod lsp_client;
mod support;

use lsp_client::LspClient;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use support::Must;
use tempfile::tempdir;
use url::Url;

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
    let directory = tempdir().must_be("real analyzer workspace");
    let project = directory.path().join("genuine analyzer project 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    // Real language/unneeded_plus and layout/format rules: preserve independent
    // needless-binder checks while four-space indentation elicits a genuine fix.
    let source = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n    Nat.add(x, y)\n";
    let canonical = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";
    let root = project.join("main.bend");
    fs::write(&root, source).must_be("write genuine Bend source");
    fs::write(
        project.join("bend-lint.json"),
        "{\"packages\":{\"bend-lint-rules\":\"0.1.0.0\"},\"rules\":{\"bend-lint-rules/language/unneeded_plus\":{},\"bend-lint-rules/layout/format\":{}}}",
    )
    .must_be("enable genuine upstream rule");
    let cli = Command::new(&executable)
        .args(&arguments)
        .arg(&root)
        .arg("--json")
        .args(["--jobs", "1"])
        .current_dir(&project)
        .output()
        .must_be("run genuine bend-lint CLI without fixes");
    assert!(
        cli.status.success(),
        "real analyzer failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report: Value =
        serde_json::from_slice(&cli.stdout).must_be("genuine upstream CLI JSON envelope");
    assert_eq!(report["ok"], true);
    assert!(report["suppressed"].is_array());
    let expected = report["findings"]
        .as_array()
        .must_be("genuine findings")
        .iter()
        .find(|finding| finding["code"] == "bend-lint-rules/language/unneeded_plus")
        .must_be("genuine analyzer detects needless +x");
    assert!(
        expected["message"]
            .as_str()
            .must_be("genuine rule message")
            .starts_with("+x is used at most once")
    );
    assert_eq!(expected["range"]["start"], json!({"line":2,"character":7}));
    assert_eq!(expected["range"]["end"], json!({"line":2,"character":8}));
    let format = report["findings"]
        .as_array()
        .must_be("genuine findings")
        .iter()
        .find(|finding| finding["code"] == "bend-lint-rules/layout/format")
        .must_be("genuine analyzer detects noncanonical indentation");
    assert_eq!(format["message"], "Apply canonical formatting.");
    assert_eq!(format["range"]["start"], json!({"line":3,"character":2}));
    let reported_fixes = format["fixes"].as_array().must_be("real reported fixes");
    assert_eq!(reported_fixes.len(), 1);
    let reported_fix = &reported_fixes[0];
    assert_eq!(reported_fix["title"], "Format the file");
    assert_eq!(reported_fix["applicability"], "dangerous");
    let reported_edits = reported_fix["edits"].as_array().must_be("real reported edits");
    assert_eq!(reported_edits.len(), 1);
    let reported_edit = &reported_edits[0];
    let target = Path::new(reported_edit["path"].as_str().must_be("reported edit path"));
    let target = if target.is_absolute() { target.to_path_buf() } else { project.join(target) };
    assert_eq!(
        fs::canonicalize(target).must_be("actual reported edit target"),
        fs::canonicalize(&root).must_be("actual source path")
    );
    let entire_file = json!({
        "start":{"line":0,"character":0},
        "end":{"line":4,"character":0}
    });
    assert_eq!(reported_edit["range"], entire_file);
    assert_eq!(reported_edit["text"], canonical);
    let mut command = env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
        || Command::new(env!("CARGO_BIN_EXE_bend2-lsp")),
        Command::new,
    );
    command.current_dir(&project).stderr(Stdio::inherit());
    let mut client = LspClient::spawn(command);
    client.initialize(&project);
    let mut settings = json!({"bend":{"lint":{
        "enabled":true,"path":executable,"arguments":arguments,
        "dangerousCodeActions":true,"timeoutMs":120_000
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
    let document = uri(&root);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
        "uri":document,"languageId":"bend","version":1,"text":source}}),
    );
    assert!(
        client
            .receive_matching(Duration::from_millis(350), |message| {
                lint_packet(message, &document)
            })
            .is_none(),
        "even genuine rules must wait for save"
    );
    client.notify(
        "textDocument/didSave",
        json!({"textDocument":{"uri":document}}),
    );
    let packet = client
        .receive_matching(Duration::from_secs(150), |message| {
            lint_packet(message, &document)
        })
        .must_be("real analyzer finding arrives through stdio LSP");
    assert_eq!(packet["params"]["version"], 1);
    let diagnostics = packet["params"]["diagnostics"]
        .as_array()
        .must_be("real LSP diagnostics");
    let actual = diagnostics
        .iter()
        .find(|item| {
            item["source"] == "bend-lint"
                && item["code"] == "bend-lint-rules/language/unneeded_plus"
        })
        .must_be("same genuine rule appears in editor");
    assert_eq!(actual["message"], expected["message"]);
    assert_eq!(actual["range"], expected["range"]);
    assert_eq!(
        actual["severity"], 4,
        "upstream Hint is LSP Hint, not a compiler error"
    );
    let format_actual = diagnostics
        .iter()
        .find(|item| item["source"] == "bend-lint" && item["code"] == format["code"])
        .must_be("same genuine format finding appears in editor");
    assert_eq!(format_actual["message"], format["message"]);
    assert_eq!(format_actual["range"], format["range"]);
    assert_eq!(format_actual["severity"], 2, "upstream Warning is LSP Warning");
    let automatic = client.request("textDocument/codeAction", json!({
        "textDocument":{"uri":document},"range":format_actual["range"],
        "context":{"diagnostics":[format_actual],"triggerKind":2,
            "only":["quickfix.bend-lint.dangerous"]}
    }));
    assert_eq!(automatic["result"], json!([]), "no automatic dangerous action");
    let response = client.request("textDocument/codeAction", json!({
        "textDocument":{"uri":document},"range":format_actual["range"],
        "context":{"diagnostics":[format_actual],"triggerKind":1,
            "only":["quickfix.bend-lint.dangerous"]}
    }));
    let actions = response["result"].as_array().must_be("explicit genuine actions");
    assert_eq!(actions.len(), 1, "only the actual reported dangerous fix");
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    assert_eq!(action["isPreferred"], false);
    assert!(
        action["title"].as_str().must_be("dangerous title")
            .starts_with("Dangerous bend-lint fix (may change behavior): ")
    );
    assert!(action["title"].as_str().must_be("reported title").ends_with("Format the file"));
    assert_eq!(action["diagnostics"][0]["code"], format["code"]);
    assert!(action["command"].is_null(), "only client-applied edits");
    assert!(action["edit"]["changes"].is_null(), "never unversioned changes");
    let changes = action["edit"]["documentChanges"].as_array().must_be("versioned edit");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["textDocument"]["uri"], document);
    assert_eq!(changes[0]["textDocument"]["version"], 1);
    let edits = changes[0]["edits"].as_array().must_be("actual replacement");
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["range"], reported_edit["range"]);
    assert_eq!(edits[0]["newText"], reported_edit["text"]);
    assert_eq!(
        fs::read_to_string(&root).must_be("unchanged genuine source"),
        source
    );
    // A client explicitly accepts the full-file edit only after the above
    // independent bounds, replacement and document-version assertions.
    let mut applied = source.to_owned();
    let end = applied.len();
    applied.replace_range(0..end, edits[0]["newText"].as_str().must_be("replacement text"));
    assert_eq!(applied, canonical, "independently expected two-space formatting");
    fs::write(&root, &applied).must_be("user explicitly applies the versioned edit");
    assert_eq!(fs::read_to_string(&root).must_be("user-applied source"), canonical);
    client.notify("textDocument/didChange", json!({
        "textDocument":{"uri":document,"version":2},
        "contentChanges":[{"text":applied}]
    }));
    client.finish();
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
