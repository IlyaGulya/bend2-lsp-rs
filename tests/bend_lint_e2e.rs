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
// absolute installed src/linter/seam.ts path (not bunx or a package manager).
// The tested upstream schema is revision
// 02ef84b38b8c06cbd607e270c4930e633e9dab0f, not the older PR1389 CLI.
#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn genuine_analyzer_matches_its_cli_report_through_saved_stdio_document() {
    let executable = PathBuf::from(
        env::var_os("BEND2_LINT_TEST_PATH")
            .must_be("set BEND2_LINT_TEST_PATH to the genuine installed analyzer or Bun"),
    );
    assert!(executable.is_absolute(), "use an explicit absolute executable");
    let arguments: Vec<String> = env::var("BEND2_LINT_TEST_ARGUMENTS").map_or_else(
        |_| Vec::new(),
        |value| serde_json::from_str(&value).must_be("JSON analyzer argument array"),
    );
    assert!(!arguments.iter().any(|argument| argument.starts_with("--fix")));
    let directory = tempdir().must_be("real analyzer workspace");
    let project = directory.path().join("genuine analyzer project 🦀");
    fs::create_dir(&project).must_be("create genuine project");
    // Source and expected rule identity come from the upstream rule's own
    // language/unneeded_plus.test.ts, not a replay executable.
    let source = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";
    let root = project.join("main.bend");
    fs::write(&root, source).must_be("write genuine Bend source");
    fs::write(
        project.join("bend-lint.json"),
        "{\"packages\":{\"bend-lint-rules\":\"0.1.0.0\"},\"rules\":{\"bend-lint-rules/language/unneeded_plus\":{}}}",
    )
    .must_be("enable genuine upstream rule");
    let cli = Command::new(&executable)
        .args(&arguments)
        .arg(&root)
        .arg("--json")
        .current_dir(&project)
        .output()
        .must_be("run genuine bend-lint CLI without fixes");
    assert!(
        cli.status.success(),
        "real analyzer failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let report: Value = serde_json::from_slice(&cli.stdout)
        .must_be("genuine upstream CLI JSON envelope");
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
    let mut command = env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
        || Command::new(env!("CARGO_BIN_EXE_bend2-lsp")),
        Command::new,
    );
    command.current_dir(&project).stderr(Stdio::inherit());
    let mut client = LspClient::spawn(command);
    client.initialize(&project);
    let mut settings = json!({"bend":{"lint":{
        "enabled":true,"path":executable,"arguments":arguments,
        "dangerousCodeActions":false,"timeoutMs":120_000
    }}});
    if let Some(compiler) = env::var_os("BEND2_COMPILER_TEST_PATH") {
        let compiler = PathBuf::from(compiler);
        assert!(compiler.is_absolute());
        settings["bend2-lsp"] = json!({"compilerPath":compiler});
    }
    client.notify("workspace/didChangeConfiguration", json!({"settings":settings}));
    let document = uri(&root);
    client.notify("textDocument/didOpen", json!({"textDocument":{
        "uri":document,"languageId":"bend","version":1,"text":source}}));
    assert!(
        client.receive_matching(Duration::from_millis(350), |message| {
            lint_packet(message, &document)
        }).is_none(),
        "even genuine rules must wait for save"
    );
    client.notify("textDocument/didSave", json!({"textDocument":{"uri":document}}));
    let packet = client
        .receive_matching(Duration::from_secs(150), |message| lint_packet(message, &document))
        .must_be("real analyzer finding arrives through stdio LSP");
    assert_eq!(packet["params"]["version"], 1);
    let diagnostics = packet["params"]["diagnostics"]
        .as_array()
        .must_be("real LSP diagnostics");
    let actual = diagnostics.iter().find(|item| {
        item["source"] == "bend-lint" && item["code"] == "bend-lint-rules/language/unneeded_plus"
    }).must_be("same genuine rule appears in editor");
    assert_eq!(actual["message"], expected["message"]);
    assert_eq!(actual["range"], expected["range"]);
    assert_eq!(actual["severity"], 4, "upstream Hint is LSP Hint, not a compiler error");
    assert_eq!(fs::read_to_string(&root).must_be("unchanged genuine source"), source);
    client.finish();
}

fn uri(path: &Path) -> String {
    Url::from_file_path(path).must_be("real file URI").to_string()
}

fn lint_packet(message: &Value, document: &str) -> bool {
    message["method"] == "textDocument/publishDiagnostics"
        && message["params"]["uri"] == document
        && message["params"]["diagnostics"].as_array().is_some_and(|items| {
            items.iter().any(|item| item["source"] == "bend-lint")
        })
}
