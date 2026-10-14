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

fn uri(path: &Path) -> String {
    Url::from_file_path(path)
        .must_be("native compiler document URI")
        .to_string()
}

fn compiler() -> PathBuf {
    let path = PathBuf::from(
        env::var_os("BEND2_COMPILER_TEST_PATH")
            .must_be("set BEND2_COMPILER_TEST_PATH to the genuine pinned Bend compiler"),
    );
    let version = Command::new(&path)
        .arg("version")
        .output()
        .must_be("probe real compiler");
    assert!(version.status.success());
    assert!(
        matches!(
            String::from_utf8(version.stdout)
                .must_be("compiler version UTF-8")
                .trim(),
            "bend 2.0.36" | "bend 2.0.37"
        ),
        "use a genuine compiler from the E2E provisioning pins"
    );
    path
}

fn server_command() -> Command {
    env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
        || Command::new(env!("CARGO_BIN_EXE_bend2-lsp")),
        Command::new,
    )
}

fn diagnostics(client: &mut LspClient, document: &str, version: Option<i32>) -> Vec<Value> {
    let message = client
        .receive_matching(Duration::from_secs(30), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == document
                && version.is_none_or(|version| message["params"]["version"] == version)
        })
        .must_be("receive real native compiler diagnostics");
    message["params"]["diagnostics"]
        .as_array()
        .must_be("native diagnostic array")
        .clone()
}

#[test]
fn conflicting_base_import_reports_the_cause_at_the_import_instead_of_a_banner() {
    let compiler = compiler();
    let directory = tempdir().must_be("native compiler workspace");
    let dependency = directory.path().join("base.bend");
    fs::write(&dependency, "type Nat is Data:\n  Zero{}\n").must_be("write conflicting module");
    let root = directory.path().join("main.bend");
    let source =
        "import Base\nimport ./base.bend as Duplicate\n\ndef main(x: U32) -> U32:\n  Color.Red\n";
    fs::write(&root, source).must_be("write native root before discovery");
    let mut command = server_command();
    command.stderr(Stdio::null()).env("PATH", directory.path());
    let mut client = LspClient::spawn(command);
    client.initialize(directory.path());
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"bend2-lsp":{"compilerPath":compiler}}}),
    );
    let root_uri = uri(&root);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":root_uri,"languageId":"bend","version":1,"text":source}}),
    );
    let root_errors = diagnostics(&mut client, &root_uri, Some(1));
    let dependency_errors = diagnostics(&mut client, &uri(&dependency), None);
    assert_eq!(
        dependency_errors.len(),
        1,
        "retain the actual duplicate declaration error"
    );
    assert!(
        dependency_errors[0]["message"]
            .as_str()
            .must_be("duplicate message")
            .contains("duplicate declaration: Nat")
    );
    assert_eq!(
        root_errors.len(),
        1,
        "one causal import diagnostic, not cascaded dependent errors"
    );
    assert_eq!(root_errors[0]["code"], "compiler-import-error");
    assert_eq!(
        root_errors[0]["range"],
        json!({"start":{"line":1,"character":7},"end":{"line":1,"character":18}})
    );
    assert_eq!(
        root_errors[0]["relatedInformation"][0]["location"]["uri"],
        uri(&dependency)
    );
    assert_eq!(
        root_errors[0]["relatedInformation"][0]["location"]["range"],
        dependency_errors[0]["range"]
    );
    let repaired_import = "import Base\n\ndef main(x: U32) -> U32:\n  Color.Red\n";
    client.notify("textDocument/didChange", json!({"textDocument":{"uri":root_uri,"version":2},"contentChanges":[{"text":repaired_import}]}));
    let root_errors = diagnostics(&mut client, &root_uri, Some(2));
    assert_eq!(
        root_errors.len(),
        1,
        "report the source error once its dependency can be checked"
    );
    assert_eq!(root_errors[0]["code"], "compiler");
    assert!(
        root_errors[0]["message"]
            .as_str()
            .must_be("source error")
            .contains("Color.Red")
    );
    assert_eq!(
        root_errors[0]["range"],
        json!({"start":{"line":3,"character":2},"end":{"line":3,"character":11}})
    );
    assert_eq!(
        diagnostics(&mut client, &uri(&dependency), None),
        Vec::<Value>::new(),
        "clear dependency diagnostics when the bad import is removed"
    );
    let repaired_source = "import Base\n\ndef main(x: U32) -> U32:\n  x\n";
    client.notify("textDocument/didChange", json!({"textDocument":{"uri":root_uri,"version":3},"contentChanges":[{"text":repaired_source}]}));
    assert_eq!(
        diagnostics(&mut client, &root_uri, Some(3)),
        Vec::<Value>::new(),
        "clear the source error after an unsaved correction"
    );
    client.finish();
}

#[test]
fn compiler_owned_base_stays_exempt_but_user_base_is_checked() {
    let compiler = compiler();
    let directory = tempdir().must_be("Base provenance workspace");
    let root = directory.path().join("main.bend");
    let source = "import Base\n\ndef main(x: U32) -> U32:\n  x\n";
    fs::write(&root, source).must_be("write Base consumer before discovery");
    let mut command = server_command();
    command.stderr(Stdio::null()).env("PATH", directory.path());
    let mut client = LspClient::spawn(command);
    client.initialize(directory.path());
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"bend2-lsp":{"compilerPath":compiler}}}),
    );
    let root_uri = uri(&root);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":root_uri,"languageId":"bend","version":1,"text":source}}),
    );
    assert_eq!(
        diagnostics(&mut client, &root_uri, Some(1)),
        Vec::<Value>::new()
    );
    let definition = client.request(
        "textDocument/definition",
        json!({"textDocument":{"uri":root_uri},"position":{"line":2,"character":12}}),
    );
    let builtin_uri = definition["result"]["uri"]
        .as_str()
        .must_be("compiler-owned U32 definition URI");
    let builtin_path = Url::parse(builtin_uri)
        .must_be("compiler Base URL")
        .to_file_path()
        .must_be("compiler Base file");
    let builtin_source = fs::read_to_string(&builtin_path).must_be("read genuine compiler Base");
    client.notify("textDocument/didOpen", json!({"textDocument":{"uri":builtin_uri,"languageId":"bend","version":1,"text":builtin_source}}));
    assert_eq!(
        diagnostics(&mut client, builtin_uri, Some(1)),
        Vec::<Value>::new(),
        "compiler-owned Base must not be checked as an ordinary module"
    );
    let unfilled_law = "law Handle: Type\n";
    client.notify("textDocument/didChange", json!({"textDocument":{"uri":builtin_uri,"version":2},"contentChanges":[{"text":unfilled_law}]}));
    assert_eq!(
        diagnostics(&mut client, builtin_uri, Some(2)),
        Vec::<Value>::new(),
        "compiler ownership survives unsaved changes"
    );
    let user_path = directory.path().join("Base.bend");
    fs::write(&user_path, unfilled_law).must_be("write ordinary user Base module");
    let user_uri = uri(&user_path);
    client.notify("textDocument/didOpen", json!({"textDocument":{"uri":user_uri,"languageId":"bend","version":1,"text":unfilled_law}}));
    let user_errors = diagnostics(&mut client, &user_uri, Some(1));
    assert_eq!(
        user_errors.len(),
        1,
        "a filename must not grant compiler ownership"
    );
    assert_eq!(user_errors[0]["code"], "compiler");
    assert!(
        user_errors[0]["message"]
            .as_str()
            .must_be("user Base rejection")
            .contains("unfilled law")
    );
    assert_eq!(
        user_errors[0]["range"],
        json!({"start":{"line":0,"character":0},"end":{"line":0,"character":16}})
    );
    client.finish();
}

#[test]
fn transitive_import_links_original_cause_and_keeps_independent_source_error() {
    let compiler = compiler();
    let directory = tempdir().must_be("transitive native compiler workspace");
    let dependency = directory.path().join("base.bend");
    fs::write(&dependency, "type Nat is Data:\n  Zero{}\n")
        .must_be("write conflicting transitive module");
    let middle = directory.path().join("middle.bend");
    fs::write(&middle, "import ./base.bend as Dependency\n").must_be("write intermediate import");
    let root = directory.path().join("main.bend");
    let source = "import Base\nimport ./middle.bend as Middle\n\ndef independent() -> Type:\n  Missing.Type\n";
    fs::write(&root, source).must_be("write native root before discovery");
    let mut command = server_command();
    command.stderr(Stdio::null()).env("PATH", directory.path());
    let mut client = LspClient::spawn(command);
    client.initialize(directory.path());
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"bend2-lsp":{"compilerPath":compiler}}}),
    );
    let root_uri = uri(&root);
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":root_uri,"languageId":"bend","version":1,"text":source}}),
    );
    let root_errors = diagnostics(&mut client, &root_uri, Some(1));
    let dependency_errors = diagnostics(&mut client, &uri(&dependency), None);
    let middle_errors = diagnostics(&mut client, &uri(&middle), None);
    assert_eq!(
        root_errors.len(),
        2,
        "one independent error and one causal import, not blocked-definition errors"
    );
    let import = root_errors
        .iter()
        .find(|error| error["code"] == "compiler-import-error")
        .must_be("root transitive import error");
    let independent = root_errors
        .iter()
        .find(|error| error["code"] == "compiler")
        .must_be("independent source error");
    assert_eq!(
        import["range"],
        json!({"start":{"line":1,"character":7},"end":{"line":1,"character":20}})
    );
    assert_eq!(
        independent["range"],
        json!({"start":{"line":4,"character":2},"end":{"line":4,"character":14}})
    );
    assert!(
        independent["message"]
            .as_str()
            .must_be("independent error message")
            .contains("Missing.Type")
    );
    assert_eq!(dependency_errors.len(), 1);
    assert_eq!(
        import["relatedInformation"]
            .as_array()
            .must_be("transitive cause links")
            .len(),
        1
    );
    assert_eq!(
        import["relatedInformation"][0]["location"]["uri"],
        uri(&dependency)
    );
    assert_eq!(
        import["relatedInformation"][0]["location"]["range"],
        dependency_errors[0]["range"]
    );
    assert_eq!(
        middle_errors.len(),
        1,
        "retain the immediate importer location"
    );
    assert_eq!(
        middle_errors[0]["range"],
        json!({"start":{"line":0,"character":7},"end":{"line":0,"character":18}})
    );
    assert_eq!(
        middle_errors[0]["relatedInformation"], import["relatedInformation"],
        "both importers link the original source error, not a synthesized cascade"
    );
    client.finish();
}
