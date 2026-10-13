#![cfg(all(target_os = "macos", target_arch = "aarch64"))]

#[path = "support/lsp_client.rs"]
pub mod lsp_client;
mod support;

use lsp_client::LspClient;
use serde_json::{Value, json};
use std::{
    env, fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use support::Must;
use tempfile::tempdir;
use url::Url;

fn assert_unfilled_law_range(source: &str, expected_range: &Value) {
    let compiler = PathBuf::from(
        env::var_os("BEND2_COMPILER_TEST_PATH")
            .must_be("set BEND2_COMPILER_TEST_PATH to the genuine pinned Bend compiler"),
    );
    let version = Command::new(&compiler)
        .arg("version")
        .output()
        .must_be("probe real compiler");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout)
            .must_be("compiler version UTF-8")
            .trim(),
        "bend 2.0.36"
    );

    let directory = tempdir().must_be("native law workspace");
    let root = directory.path().join("main.bend");
    fs::write(&root, source).must_be("write native law source before discovery");
    let root_uri = Url::from_file_path(&root)
        .must_be("native law document URI")
        .to_string();
    let mut command = env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
        || Command::new(env!("CARGO_BIN_EXE_bend2-lsp")),
        Command::new,
    );
    command.stderr(Stdio::null()).env("PATH", directory.path());
    let mut client = LspClient::spawn(command);
    client.initialize(directory.path());
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"bend2-lsp":{"compilerPath":compiler}}}),
    );
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":root_uri,"languageId":"bend","version":1,"text":source}}),
    );
    let message = client
        .receive_matching(Duration::from_secs(30), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == root_uri
                && message["params"]["version"] == 1
        })
        .must_be("receive real native law diagnostics");
    let diagnostics = message["params"]["diagnostics"]
        .as_array()
        .must_be("native law diagnostic array");
    assert_eq!(
        diagnostics.len(),
        1,
        "retain only the actual law rejection: {diagnostics:?}"
    );
    assert_eq!(diagnostics[0]["code"], "compiler");
    assert!(
        diagnostics[0]["message"]
            .as_str()
            .must_be("native law rejection message")
            .contains("unfilled law"),
        "retain native incomplete-proof rejection: {diagnostics:?}"
    );
    assert_eq!(
        diagnostics[0]["range"], *expected_range,
        "underline the law, not its trailing comments or independent definition"
    );
    client.finish();
}

#[test]
fn unfilled_law_range_ends_at_type_before_comments_and_independent_definition() {
    let source = "import Base\n\nlaw Handle: Type\n\n# A comment belongs to neither declaration.\n# Another adjacent comment.\n\ndef main(x: U32) -> U32:\n  x\n";
    assert_unfilled_law_range(
        source,
        &json!({"start":{"line":2,"character":0},"end":{"line":2,"character":16}}),
    );
}

#[test]
fn multiline_unfilled_law_range_ends_at_final_type_token_before_comments() {
    let source = "import Base\n\nlaw Divide:\n  for a: U32\n  for b: U32\n  U32\n\n# Adjacent comments are not part of the law.\n# Nor is the following independent definition.\n\ndef main(x: U32) -> U32:\n  x\n";
    assert_unfilled_law_range(
        source,
        &json!({"start":{"line":2,"character":0},"end":{"line":5,"character":5}}),
    );
}
