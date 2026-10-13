#[path = "support/lsp_client.rs"]
pub mod lsp_client;
mod support;

use lsp_client::LspClient;
use serde_json::json;
use std::{
    env, fs,
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};
use support::Must;
use tempfile::Builder;
use url::Url;

#[test]
fn genuine_compiler_reports_both_independent_errors_at_their_literals() {
    let compiler = PathBuf::from(
        env::var_os("BEND2_COMPILER_TEST_PATH")
            .must_be("set BEND2_COMPILER_TEST_PATH to the genuine pinned Bend compiler"),
    );
    let directory = Builder::new()
        .prefix("bend recovery 🦀 ")
        .tempdir()
        .must_be("genuine compiler workspace");
    let root = directory.path().join("main 🦀.bend");
    let source =
        "import Base\n\ndef first() -> U32:\n  \"bad\"\n\ndef second() -> U32:\n  \"wrong\"\n";
    fs::write(&root, source).must_be("write genuine compiler source before discovery");
    let root_uri = Url::from_file_path(&root)
        .must_be("genuine compiler document URI")
        .to_string();
    let mut command = env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
        || Command::new(env!("CARGO_BIN_EXE_bend2-lsp")),
        Command::new,
    );
    command
        .stderr(Stdio::null())
        .env("PATH", directory.path())
        .env("TMPDIR", directory.path())
        .env("TMP", directory.path())
        .env("TEMP", directory.path());
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
        .must_be("receive both genuine compiler errors");
    let errors = message["params"]["diagnostics"]
        .as_array()
        .must_be("genuine compiler diagnostic array");
    assert_eq!(
        errors.len(),
        2,
        "preserve both independent errors: {errors:?}"
    );
    let mut ranges = errors
        .iter()
        .map(|error| {
            assert_eq!(error["severity"], 1);
            assert!(
                error["message"]
                    .as_str()
                    .must_be("compiler error message")
                    .contains("U32"),
                "retain the native expected type: {error:?}"
            );
            error["range"].clone()
        })
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| range["start"]["line"].as_u64());
    assert_eq!(
        ranges,
        vec![
            json!({"start":{"line":3,"character":2},"end":{"line":3,"character":7}}),
            json!({"start":{"line":6,"character":2},"end":{"line":6,"character":9}}),
        ],
        "locate each error on its own string literal"
    );
    client.finish();
}
