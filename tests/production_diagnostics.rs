#[path = "support/lsp_client.rs"]
mod lsp_client;
mod support;

use lsp_client::LspClient;
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};
use support::Must;
use tempfile::tempdir;
use url::Url;

fn client(root: &Path) -> LspClient {
    let executable = std::env::var_os("BEND2_LSP_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_bend2-lsp").into());
    let mut command = Command::new(executable);
    command
        .current_dir(root)
        .stderr(Stdio::null())
        .env("PATH", root)
        .env_remove("BEND_LIB")
        .env_remove("BEND2_LSP_TRACE")
        .env_remove("BEND2_LSP_COMPILER_METRICS_FILE");
    let mut client = LspClient::spawn(command);
    client.initialize(root);
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"bend2-lsp":{
            "compilerPath":root.join("unavailable-bend").to_string_lossy()
        }}}),
    );
    client
}

#[test]
fn local_export_counts_open_source_without_disclosing_it_or_interrupting_queries() {
    let root = tempdir().must_be("private diagnostics workspace");
    let source = "def SECRET_CONTENT_MARKER() -> U32:\n  41\n";
    let document =
        Url::from_file_path(root.path().join("private-document.bend")).must_be("source URI");
    let destination = root.path().join("diagnostics.json");
    let destination_uri = Url::from_file_path(&destination).must_be("export URI");
    let mut client = client(root.path());
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":source
        }}),
    );
    let before = client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":document}}),
    );
    assert_eq!(before["result"][0]["name"], "SECRET_CONTENT_MARKER");

    client.request(
        "workspace/executeCommand",
        json!({"command":"bend2.diagnostics.export","arguments":[{"uri":destination_uri}]}),
    );
    let raw = fs::read_to_string(&destination).must_be("read local diagnostic snapshot");
    let report: Value = serde_json::from_str(&raw).must_be("parse diagnostic snapshot");
    assert_eq!(report["schemaVersion"], 1);
    assert_eq!(report["server"]["pid"], client.process_id());
    assert_eq!(report["workspace"]["openDocuments"], 1);
    assert_eq!(report["storage"]["snapshots"]["sourceBytes"], source.len());
    assert!(
        !raw.contains("SECRET_CONTENT_MARKER"),
        "source leaked: {raw}"
    );
    assert!(
        !raw.contains(root.path().to_str().must_be("workspace path")),
        "absolute path leaked: {raw}"
    );
    assert!(!raw.contains(document.as_str()), "source URI leaked: {raw}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&destination)
                .must_be("export metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    let after = client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":document}}),
    );
    assert_eq!(after["result"], before["result"]);
    client.finish();
}

#[cfg(unix)]
#[test]
fn diagnostic_sampling_stops_on_sigterm() {
    let root = tempdir().must_be("diagnostics termination workspace");
    let destination =
        Url::from_file_path(root.path().join("diagnostics.json")).must_be("export URI");
    let mut client = client(root.path());
    client.request(
        "workspace/executeCommand",
        json!({"command":"bend2.diagnostics.export","arguments":[{"uri":destination}]}),
    );
    client.sigterm();
    let status = client
        .wait_timeout(std::time::Duration::from_secs(10))
        .must_be("diagnostic sampler must not keep the server alive after SIGTERM");
    assert!(status.success(), "server exited with {status}");
}

#[test]
fn manual_source_action_exports_the_running_server_without_enabling_lint() {
    let root = tempdir().must_be("diagnostics source action workspace");
    let document = Url::from_file_path(root.path().join("main.bend")).must_be("source URI");
    let mut client = client(root.path());
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":"def main() -> U32:\n  42\n"
        }}),
    );
    let params = json!({
        "textDocument":{"uri":document},
        "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},
        "context":{"diagnostics":[],"only":["source.bend2Diagnostics.export"],"triggerKind":1}
    });
    let manual = client.request("textDocument/codeAction", params.clone());
    let action = manual["result"]
        .as_array()
        .and_then(|actions| {
            actions
                .iter()
                .find(|action| action["kind"] == "source.bend2Diagnostics.export")
        })
        .must_be("a manual source action must expose production diagnostics without lint");
    let exported = client.request("workspace/executeCommand", action["command"].clone());
    let uri = exported["result"]["uri"]
        .as_str()
        .must_be("generated snapshot URI");
    let path = Url::parse(uri)
        .must_be("generated file URI")
        .to_file_path()
        .must_be("generated local path");
    let report: Value = serde_json::from_slice(&fs::read(&path).must_be("generated report"))
        .must_be("generated report JSON");
    assert_eq!(report["server"]["pid"], client.process_id());
    assert_eq!(report["workspace"]["openDocuments"], 1);
    let mut automatic_params = params;
    automatic_params["context"]["triggerKind"] = json!(2);
    let automatic = client.request("textDocument/codeAction", automatic_params);
    assert_eq!(
        automatic["result"],
        json!([]),
        "export is an explicit action, never automatic"
    );
    fs::remove_file(&path).must_be("remove generated diagnostic report");
    fs::remove_dir(path.parent().must_be("generated report directory"))
        .must_be("remove generated private report directory");
    client.finish();
}
