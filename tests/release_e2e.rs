#[path = "support/lsp_client.rs"]
mod lsp_client;
mod support;

use lsp_client::LspClient;
use serde_json::{Value, json};
use std::{
    fs,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use support::Must;
use tempfile::tempdir;
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(15);

fn compiler_fixture(directory: &Path) -> PathBuf {
    let executable = directory.join(format!("portable compiler{}", std::env::consts::EXE_SUFFIX));
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let output = Command::new(rustc)
        .arg("--edition=2024")
        .arg("-Dwarnings")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/release_compiler.rs"))
        .arg("-o")
        .arg(&executable)
        .output()
        .must_be("compile portable child executable fixture");
    assert!(
        output.status.success(),
        "fixture compilation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

fn spawn_client(workspace: &Path, trace: Option<&Path>) -> LspClient {
    let binary = std::env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_bend2-lsp")),
        PathBuf::from,
    );
    let binary = fs::canonicalize(binary).must_be("resolve release LSP executable");
    let mut command = Command::new(binary);
    command
        .current_dir(workspace)
        .stderr(Stdio::inherit())
        .env_remove("BEND_LIB")
        .env_remove("BEND2_LSP_TRACE")
        .env_remove("BEND2_LSP_COMPILER_METRICS_FILE");
    if let Some(trace) = trace {
        command.env("BEND2_LSP_TRACE", trace);
    }
    let client = LspClient::spawn(command);
    assert_ne!(client.process_id(), 0);
    client
}

fn configure_compiler(client: &mut LspClient, executable: &Path, control: &Path) {
    client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"bend2-lsp":{
            "compilerPath":executable,
            "compilerArguments":[control]
        }}}),
    );
}

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path).must_be("file URI").to_string()
}

fn open(client: &mut LspClient, uri: &str, source: &str) {
    client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":source}}),
    );
}

fn change(client: &mut LspClient, uri: &str, version: i32, source: &str) {
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":source}]}),
    );
}

fn diagnostics(client: &mut LspClient, uri: &str, version: i32) -> Value {
    client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == uri
                && message["params"]["version"] == version
        })
        .must_be("diagnostics for current revision")["params"]["diagnostics"]
        .clone()
}

fn range(line: u32, start: u32, end: u32) -> Value {
    json!({"start":{"line":line,"character":start},"end":{"line":line,"character":end}})
}

fn hierarchy_item(name: &str, uri: &str, line: u32, end: u32) -> Value {
    json!({
        "name":name,"kind":12,"uri":uri,
        "range":{"start":{"line":line,"character":0},"end":{"line":line+1,"character":3}},
        "selectionRange":range(line, 4, end)
    })
}

#[test]
fn release_stdio_queries_observe_incremental_unsaved_workspace() {
    let temp = tempdir().must_be("release E2E directory");
    let workspace = temp.path().join("workspace with spaces");
    fs::create_dir(&workspace).must_be("create workspace");
    let compiler = compiler_fixture(temp.path());
    let main_path = workspace.join("main.bend");
    let dependency_path = workspace.join("dep.bend");
    let disk_main = "import dep.bend as Dep\ndef main: U32\n  Dep.old(1)\n# Dep.clamp in comment\n";
    let disk_dependency = "def old(x: U32) -> U32:\n  x\n";
    fs::write(&main_path, disk_main).must_be("write root source");
    fs::write(&dependency_path, disk_dependency).must_be("write dependency source");
    let main_uri = file_uri(&main_path);
    let dependency_uri = file_uri(&dependency_path);
    let mut client = spawn_client(&workspace, None);
    initialize_capabilities(&mut client, &workspace);
    configure_compiler(&mut client, &compiler, temp.path());
    open(&mut client, &main_uri, disk_main);
    open(
        &mut client,
        &dependency_uri,
        "def clamp(x: U32) -> U32:\n  x\n",
    );
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":main_uri,"version":2},"contentChanges":[{
            "range":range(0, 0, 0),"text":"# \u{10400} prefix\n"
        }]}),
    );
    client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":main_uri,"version":3},"contentChanges":[{
            "range":range(3, 6, 9),"text":"clamp"
        }]}),
    );
    let responses = query_workspace(&mut client, &main_uri, &dependency_uri);
    assert_workspace_queries(&responses, &main_uri, &dependency_uri);
    assert_eq!(diagnostics(&mut client, &main_uri, 3), json!([]));
    client.finish();
    assert_eq!(
        fs::read_to_string(&main_path).must_be("read unchanged root"),
        disk_main
    );
    assert_eq!(
        fs::read_to_string(&dependency_path).must_be("read unchanged dependency"),
        disk_dependency
    );
}

fn initialize_capabilities(client: &mut LspClient, workspace: &Path) {
    let root_uri = Url::from_directory_path(workspace).must_be("workspace URI");
    let initialized = client.request(
        "initialize",
        json!({"processId":null,"rootUri":root_uri,"capabilities":{}}),
    );
    let capabilities = &initialized["result"]["capabilities"];
    assert_eq!(capabilities["textDocumentSync"]["change"], 2);
    for capability in ["definitionProvider", "hoverProvider", "referencesProvider"] {
        assert_eq!(capabilities[capability], true, "missing {capability}");
    }
    client.notify("initialized", json!({}));
}

fn query_workspace(client: &mut LspClient, main_uri: &str, dependency_uri: &str) -> Vec<Value> {
    // Queue every query immediately, without diagnostics or a request-response
    // barrier first committing either pending document on behalf of later queries.
    let position = json!({"textDocument":{"uri":main_uri},"position":{"line":3,"character":8}});
    let declaration =
        json!({"textDocument":{"uri":dependency_uri},"position":{"line":0,"character":6}});
    let requests = [
        ("textDocument/definition", position.clone()),
        ("textDocument/hover", position.clone()),
        ("textDocument/completion", position),
        (
            "textDocument/references",
            json!({"textDocument":{"uri":dependency_uri},"position":{"line":0,"character":6},"context":{"includeDeclaration":true}}),
        ),
        (
            "textDocument/rename",
            json!({"textDocument":{"uri":dependency_uri},"position":{"line":0,"character":6},"newName":"limit"}),
        ),
        ("workspace/symbol", json!({"query":"clamp"})),
        (
            "callHierarchy/incomingCalls",
            json!({"item":hierarchy_item("clamp", dependency_uri, 0, 9)}),
        ),
        (
            "callHierarchy/outgoingCalls",
            json!({"item":hierarchy_item("main", main_uri, 2, 8)}),
        ),
        (
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":main_uri}}),
        ),
        ("textDocument/prepareCallHierarchy", declaration),
    ];
    let ids: Vec<_> = requests
        .iter()
        .map(|(method, params)| client.send_request(method, params.clone()))
        .collect();
    ids.into_iter()
        .map(|id| {
            let response = client
                .receive_matching(TIMEOUT, |message| message["id"] == id)
                .must_be("queued query response");
            assert!(response.get("error").is_none(), "query failed: {response}");
            response["result"].clone()
        })
        .collect()
}

fn assert_workspace_queries(responses: &[Value], main_uri: &str, dependency_uri: &str) {
    assert_eq!(
        responses[0],
        json!({"uri":dependency_uri,"range":range(0, 4, 9)})
    );
    assert!(
        responses[1]["contents"]["value"]
            .as_str()
            .is_some_and(|text| text.contains("def clamp(x: U32) -> U32")),
        "{}",
        responses[1]
    );
    let completions = responses[2].as_array().must_be("completion array");
    assert!(completions.iter().any(|item| item["label"] == "clamp"));
    assert!(!completions.iter().any(|item| item["label"] == "old"));
    let expected_locations = [
        json!({"uri":dependency_uri,"range":range(0, 4, 9)}),
        json!({"uri":main_uri,"range":range(3, 6, 11)}),
    ];
    let references = responses[3].as_array().must_be("reference locations");
    assert_eq!(references.len(), 2);
    for location in &expected_locations {
        assert!(
            references.contains(location),
            "missing {location}: {}",
            responses[3]
        );
    }
    assert_eq!(
        responses[4],
        json!({"changes":{
            dependency_uri:[{"range":range(0,4,9),"newText":"limit"}],
            main_uri:[{"range":range(3,6,11),"newText":"limit"}]
        }})
    );
    let symbols = responses[5].as_array().must_be("workspace symbols");
    assert_eq!(symbols.len(), 1);
    assert_eq!(symbols[0]["name"], "clamp");
    assert_eq!(symbols[0]["location"], expected_locations[0]);
    let incoming = responses[6].as_array().must_be("incoming calls");
    assert_eq!(incoming.len(), 1);
    assert_eq!(incoming[0]["from"]["name"], "main");
    assert_eq!(incoming[0]["from"]["uri"], main_uri);
    assert_eq!(incoming[0]["fromRanges"], json!([range(3, 6, 11)]));
    let outgoing = responses[7].as_array().must_be("outgoing calls");
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0]["to"]["name"], "clamp");
    assert_eq!(outgoing[0]["to"]["uri"], dependency_uri);
    assert_eq!(outgoing[0]["fromRanges"], incoming[0]["fromRanges"]);
    let documents = responses[8].as_array().must_be("document symbols");
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0]["name"], "main");
    assert_eq!(documents[0]["selectionRange"], range(2, 4, 8));
    assert_eq!(responses[9][0]["name"], "clamp");
}

fn wait_started(control: &Path, key: &str) -> SocketAddr {
    let path = control.join(format!("{key}.started"));
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Ok(raw) = fs::read_to_string(&path) {
            let address = raw.parse().must_be("compiler lifetime address");
            assert!(
                TcpListener::bind(address).is_err(),
                "child must own its lifetime socket"
            );
            return address;
        }
        assert!(Instant::now() < deadline, "compiler did not start {key}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_child_stopped(address: SocketAddr) {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if TcpListener::bind(address).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "compiler child still owns {address}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_flushed_trace(path: &Path) {
    let raw = fs::read_to_string(path).must_be("read finalized trace");
    let events: Vec<Value> = serde_json::from_str(&raw).must_be("parse finalized Chrome trace");
    for name in [
        "lsp.request",
        "document.update",
        "snapshot.build",
        "workspace.commit",
        "compiler.check",
        "compiler.stage",
        "compiler.child",
        "diagnostics.publish",
    ] {
        let starts = events
            .iter()
            .filter(|event| event["name"] == name && event["ph"] == "b")
            .count();
        let ends = events
            .iter()
            .filter(|event| event["name"] == name && event["ph"] == "e")
            .count();
        assert!(starts > 0, "missing {name}");
        assert_eq!(starts, ends, "trace did not close {name}");
    }
    assert!(
        !raw.contains("portable compiler rejection"),
        "trace must omit compiler content"
    );
    assert!(
        !raw.contains("# block shutdown"),
        "trace must omit source content"
    );
}

#[test]
fn release_compiler_lifecycle_cancels_children_and_flushes_tracing() {
    let temp = tempdir().must_be("compiler lifecycle directory");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).must_be("create workspace");
    let path = workspace.join("main.bend");
    let source = "def main: U32\n  1\n";
    fs::write(&path, source).must_be("write disk source");
    let uri = file_uri(&path);
    let compiler = compiler_fixture(temp.path());
    let trace = temp.path().join("trace.json");
    let mut client = spawn_client(&workspace, Some(&trace));
    client.initialize(&workspace);
    configure_compiler(&mut client, &compiler, temp.path());
    open(&mut client, &uri, &format!("# error\n{source}"));
    let errors = diagnostics(&mut client, &uri, 1);
    let errors = errors.as_array().must_be("compiler diagnostics");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["message"], "portable compiler rejection");
    assert_eq!(errors[0]["code"], "checking");
    change(&mut client, &uri, 2, source);
    assert_eq!(diagnostics(&mut client, &uri, 2), json!([]));
    assert_eq!(
        fs::read_to_string(temp.path().join("observed-source")).must_be("staged source capture"),
        source
    );
    let staged =
        fs::read_to_string(temp.path().join("observed-path")).must_be("staged path capture");
    assert_ne!(
        Path::new(&staged),
        path,
        "compiler must see isolated staging, not user files"
    );
    change(&mut client, &uri, 3, &format!("# block cancel\n{source}"));
    let cancelled = wait_started(temp.path(), "cancel");
    change(&mut client, &uri, 4, source);
    wait_child_stopped(cancelled);
    assert_eq!(diagnostics(&mut client, &uri, 4), json!([]));
    change(&mut client, &uri, 5, &format!("# block close\n{source}"));
    let closed = wait_started(temp.path(), "close");
    client.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
    wait_child_stopped(closed);
    open(&mut client, &uri, &format!("# block shutdown\n{source}"));
    let shutdown = wait_started(temp.path(), "shutdown");
    assert_eq!(
        client.request("shutdown", Value::Null)["result"],
        Value::Null
    );
    wait_child_stopped(shutdown);
    client.notify("exit", Value::Null);
    // Keep stdin open: exit must terminate the actual release process itself,
    // rather than succeeding only because the test supplies EOF.
    let status = client
        .wait_timeout(TIMEOUT)
        .must_be("exit after shutdown with stdin open");
    assert!(status.success(), "server exited with {status}");
    assert_flushed_trace(&trace);
    assert_eq!(
        fs::read_to_string(&path).must_be("disk source after shutdown"),
        source
    );
    #[cfg(unix)]
    assert_sigterm_cleanup(&workspace, &compiler, temp.path(), &uri, source);
}

#[cfg(unix)]
fn assert_sigterm_cleanup(
    workspace: &Path,
    compiler: &Path,
    control: &Path,
    uri: &str,
    source: &str,
) {
    let signal_trace = control.join("sigterm-trace.json");
    let mut client = spawn_client(workspace, Some(&signal_trace));
    client.initialize(workspace);
    configure_compiler(&mut client, compiler, control);
    open(&mut client, uri, &format!("# block sigterm\n{source}"));
    let signalled = wait_started(control, "sigterm");
    client.sigterm();
    let status = client
        .wait_timeout(TIMEOUT)
        .must_be("SIGTERM release process exit");
    assert!(status.success(), "SIGTERM exited with {status}");
    wait_child_stopped(signalled);
    let raw = fs::read_to_string(signal_trace).must_be("SIGTERM trace");
    let events: Vec<Value> = serde_json::from_str(&raw).must_be("SIGTERM finalized trace JSON");
    assert!(events.iter().any(|event| event["name"] == "compiler.child"));
}
