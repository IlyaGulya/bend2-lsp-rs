#[path = "support/lsp_client.rs"]
pub mod lsp_client;
mod support;

use lsp_client::LspClient;
use serde_json::{Value, json};
use std::{
    env, fs,
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};
use support::Must;
use tempfile::{TempDir, tempdir};
use url::Url;

const TIMEOUT: Duration = Duration::from_secs(10);
const QUIET: Duration = Duration::from_millis(350);
const SOURCE: &str = "def main(+x: U32) -> U32:\n  x\n";
const COMPILER_ERROR: &str = "Error:\nselected compiler rejection\nLocation: main\n1>| def main(+x: U32) -> U32:\n  |     ^^^^\n2 |   x\n";

fn executable() -> &'static Path {
    static FIXTURE: OnceLock<(TempDir, PathBuf)> = OnceLock::new();
    &FIXTURE
        .get_or_init(|| {
            let directory = tempdir().must_be("portable analyzer fixture directory");
            let source =
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bend_lint_process.rs");
            let executable = directory
                .path()
                .join(format!("bend lint fixture{}", env::consts::EXE_SUFFIX));
            let output = Command::new(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
                .args(["--edition=2024", "-Dwarnings"])
                .arg(source)
                .arg("-o")
                .arg(&executable)
                .output()
                .must_be("compile portable analyzer process fixture");
            assert!(
                output.status.success(),
                "fixture compilation failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            (directory, executable)
        })
        .1
}

fn uri(path: &Path) -> String {
    Url::from_file_path(path)
        .must_be("document URI")
        .to_string()
}

fn range(line: u32, start: u32, end: u32) -> Value {
    json!({"start":{"line":line,"character":start},"end":{"line":line,"character":end}})
}

// Verified against standalone 0.2.1 cli.bend Object.finding/Object.fix/end at
// 8eaa9b84a74b8e1c54c55d396a585d26bd1d41e0, not an LSP-shaped invented payload.
fn finding(path: &Path, code: &str, span: Value) -> Value {
    let mut finding = json!({"code":code,"severity":"warning","message":"remove unneeded plus",
        "path":path,"fixes":[]});
    finding["range"] = span;
    finding
}

fn fix(path: &Path, span: Value, replacement: &str) -> Value {
    let mut fix = json!({"title":"Remove +","applicability":"dangerous",
        "edits":[{"path":path,"text":replacement}]});
    fix["edits"][0]["range"] = span;
    fix
}

struct Session {
    directory: TempDir,
    control: PathBuf,
    root: PathBuf,
    document: String,
    client: LspClient,
}

impl Session {
    fn new(source: &str, dependencies: &[(&str, &str)]) -> Self {
        Self::with_server_capabilities(source, dependencies, json!({}))
    }

    fn with_server_capabilities(
        source: &str,
        dependencies: &[(&str, &str)],
        capabilities: Value,
    ) -> Self {
        let directory = tempdir().must_be("lint protocol workspace");
        let project = directory.path().join("project with spaces 🦀");
        let control = directory.path().join("control");
        fs::create_dir(&project).must_be("create lint project");
        fs::create_dir(&control).must_be("create process controls");
        fs::write(project.join("bend-lint.json"), "{\"rules\":{}}")
            .must_be("write real config shape before discovery");
        let root = project.join("main.bend");
        fs::write(&root, source).must_be("write saved root");
        for (name, text) in dependencies {
            fs::write(project.join(name), text).must_be("write saved dependency");
        }
        fs::write(
            control.join("response.json"),
            "{\"ok\":true,\"findings\":[],\"suppressed\":[],\"skipped\":[]}",
        )
        .must_be("initialize process output");
        fs::write(control.join("exit-code"), "0").must_be("initialize exit code");
        let mut command = env::var_os("BEND2_LSP_TEST_BINARY").map_or_else(
            || Command::new(env!("CARGO_BIN_EXE_bend2-lsp")),
            Command::new,
        );
        command
            .current_dir(&project)
            .stderr(Stdio::null())
            .env("PATH", &control)
            .env("BEND_LINT_FIXTURE_CONTROL", &control)
            .env_remove("BEND_LIB");
        let mut client = LspClient::spawn(command);
        client.initialize_with_capabilities(&project, capabilities);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{"compilerPath":executable()}}}),
        );
        Self {
            document: uri(&root),
            directory,
            control,
            root,
            client,
        }
    }

    fn project(&self) -> &Path {
        self.root.parent().must_be("project directory")
    }

    fn configure(&mut self, enabled: bool, dangerous: bool, timeout_ms: u32) {
        self.client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{
                "bend2-lsp":{"compilerPath":executable()},
                "bend":{"lint":{"enabled":enabled,"path":executable(),
                    "dangerousCodeActions":dangerous,"timeoutMs":timeout_ms}}
            }}),
        );
    }

    fn open(&mut self, source: &str) {
        self.client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
            "uri":self.document,"languageId":"bend","version":1,"text":source}}),
        );
    }

    fn change(&mut self, version: i32, source: &str) {
        self.client.notify(
            "textDocument/didChange",
            json!({"textDocument":{
            "uri":self.document,"version":version},"contentChanges":[{"text":source}]}),
        );
    }

    fn save(&mut self) {
        self.client.notify(
            "textDocument/didSave",
            json!({"textDocument":{"uri":self.document}}),
        );
    }

    fn response(&self, findings: Vec<Value>, suppressed: Vec<Value>, ok: bool) {
        self.response_with_skips(findings, suppressed, Vec::new(), ok);
    }

    fn response_with_skips(
        &self,
        findings: Vec<Value>,
        suppressed: Vec<Value>,
        skipped: Vec<Value>,
        ok: bool,
    ) {
        let mut response = json!({"ok":ok});
        response["findings"] = Value::Array(findings);
        response["suppressed"] = Value::Array(suppressed);
        response["skipped"] = Value::Array(skipped);
        fs::write(
            self.control.join("response.json"),
            serde_json::to_vec(&response).must_be("serialize upstream response"),
        )
        .must_be("write analyzer response");
        fs::write(self.control.join("exit-code"), if ok { "0" } else { "1" })
            .must_be("write upstream exit contract");
    }

    fn calls(&self) -> usize {
        fs::read_to_string(self.control.join("calls"))
            .map_or(0, |text| text.lines().count())
    }

    fn wait_calls(&self, count: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while self.calls() < count {
            assert!(Instant::now() < deadline, "analyzer did not run {count} time(s)");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn prepares(&self) -> usize {
        fs::read_to_string(self.control.join("prepares"))
            .map_or(0, |text| text.lines().count())
    }

    fn overlay_input(&self) -> Value {
        serde_json::from_slice(
            &fs::read(self.control.join("stdin")).must_be("recorded overlay stdin"),
        )
        .must_be("overlay stdin is one JSON object")
    }

    fn status(&mut self) -> Value {
        self.client.request(
            "workspace/executeCommand",
            json!({"command":"bend2.lint.status","arguments":[{"uri":self.document}]}),
        )["result"]
            .clone()
    }

    fn acknowledge_progress_create(&mut self) -> Value {
        let request = self
            .client
            .receive_matching(TIMEOUT, |message| {
                message["method"] == "window/workDoneProgress/create"
            })
            .must_be("the server creates a work-done progress token");
        self.client.respond(&request, Value::Null);
        request
    }

    fn progress(&mut self, kind: &str, token: &Value) -> Value {
        let kind = kind.to_owned();
        let token = token.clone();
        self.client
            .receive_matching(TIMEOUT, move |message| {
                message["method"] == "$/progress"
                    && message["params"]["token"] == token
                    && message["params"]["value"]["kind"] == kind
            })
            .must_be("matching work-done progress notification")
    }

    fn standard_response(&self) {
        self.response(
            vec![finding(
                &self.root,
                "language/unneeded_plus",
                range(0, 9, 10),
            )],
            vec![],
            true,
        );
    }

    fn lint(&mut self) -> Value {
        self.client
            .receive_matching(TIMEOUT, |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == self.document
                    && has_lint(message)
            })
            .must_be("saved document receives bend-lint diagnostics")
    }

    fn compiler(&mut self, version: i32) -> Value {
        self.client
            .receive_matching(TIMEOUT, |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == self.document
                    && message["params"]["version"] == version
                    && message["params"]["diagnostics"]
                        .as_array()
                        .is_some_and(|items| {
                            items
                                .iter()
                                .any(|item| item["message"] == "selected compiler rejection")
                        })
            })
            .must_be("selected compiler error remains observable")
    }

    fn no_lint(&mut self) {
        assert!(
            self.client.receive_matching(QUIET, has_lint).is_none(),
            "disk findings must not be published for an obsolete or dirty snapshot"
        );
    }

    fn wait_file(&self, name: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Ok(text) = fs::read_to_string(self.control.join(name)) {
                return text;
            }
            assert!(Instant::now() < deadline, "fixture did not create {name}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn block(&self) {
        fs::write(self.control.join("block"), "").must_be("block analyzer completion");
    }

    fn release(&mut self) {
        // Writing a notification into stdin does not acknowledge server receipt.
        // Keep analyzer output blocked until preceding mutations are observed.
        self.client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":self.document}}),
        );
        fs::write(self.control.join("release"), "").must_be("release analyzer completion");
    }

    fn failure(&mut self) -> Value {
        self.client
            .receive_matching(TIMEOUT, |message| {
                matches!(
                    message["method"].as_str(),
                    Some("window/logMessage" | "window/showMessage")
                ) && message["params"]["message"].as_str().is_some_and(|text| {
                    let text = text.to_ascii_lowercase();
                    text.contains("bend-lint")
                        && [
                            "fail",
                            "error",
                            "timeout",
                            "timed out",
                            "invalid",
                            "missing",
                            "not found",
                        ]
                        .iter()
                        .any(|word| text.contains(word))
                })
            })
            .must_be("analyzer failure is visible without a fake source diagnostic")
    }

    fn actions(&mut self, diagnostics: Value, trigger: u8) -> Vec<Value> {
        self.actions_at(diagnostics, trigger, range(0, 0, 24))
    }

    fn actions_at(&mut self, diagnostics: Value, trigger: u8, span: Value) -> Vec<Value> {
        let mut params = json!({
            "textDocument":{"uri":self.document},"range":span,
            "context":{"triggerKind":trigger,"only":["quickfix.bend-lint.dangerous"]}
        });
        params["context"]["diagnostics"] = diagnostics;
        self.client.request("textDocument/codeAction", params)["result"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn finish(self) {
        self.client.finish();
        // Keep workspace alive until the server has finished its shutdown.
        drop(self.directory);
    }
}

fn has_lint(message: &Value) -> bool {
    message["method"] == "textDocument/publishDiagnostics"
        && message["params"]["diagnostics"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["source"] == "bend-lint"))
}

fn socket_closed(address: &str) {
    let address: SocketAddr = address.trim().parse().must_be("fixture socket address");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_err() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "analyzer child remains alive at {address}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn lint_is_off_by_default_even_with_project_rules_and_save() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.open(SOURCE);
    session.save();
    session.no_lint();
    assert!(
        !session.control.join("calls").exists(),
        "no unsolicited executable invocation"
    );
    session.finish();
}

#[test]
fn explicit_path_without_enabled_consent_never_executes_project_rules() {
    let mut session = Session::new(SOURCE, &[]);
    session.configure(false, false, 10_000);
    session.open(SOURCE);
    session.save();
    session.no_lint();
    assert!(!session.control.join("calls").exists());
    session.finish();
}

#[test]
fn enabled_open_analyzes_overlays_in_the_nearest_project_without_fixing_disk() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let packet = session.lint();
    assert_eq!(packet["params"]["version"], 1);
    let items = packet["params"]["diagnostics"]
        .as_array()
        .must_be("diagnostics");
    let lint = items
        .iter()
        .find(|item| item["source"] == "bend-lint")
        .must_be("lint item");
    assert_eq!(lint["code"], "language/unneeded_plus");
    assert_eq!(lint["severity"], 2);
    assert_eq!(lint["range"], range(0, 9, 10));
    assert_eq!(lint["message"], "remove unneeded plus");
    let arguments = session.wait_file("arguments");
    for flag in ["--json", "--overlays", "--jobs", "1"] {
        assert!(
            arguments.lines().any(|argument| argument == flag),
            "the live check must pass {flag}: {arguments}"
        );
    }
    assert!(!arguments.contains("--fix"));
    let cwd = PathBuf::from(session.wait_file("cwd"));
    assert_eq!(
        fs::canonicalize(cwd).must_be("invocation cwd"),
        fs::canonicalize(session.project()).must_be("project")
    );
    // Preparation is a distinct invocation with its own report, not a check.
    assert_eq!(session.prepares(), 1);
    let prepare = session.wait_file("prepare-arguments");
    let prepare: Vec<_> = prepare.lines().collect();
    assert_eq!(prepare.first(), Some(&"prepare"));
    assert!(prepare.contains(&"--json"));
    assert!(prepare.contains(&"--report"));
    assert!(
        prepare
            .last()
            .is_some_and(|report| !report.starts_with("--")),
        "prepare receives a real report path: {prepare:?}"
    );
    assert_eq!(
        fs::canonicalize(session.wait_file("prepare-cwd").trim())
            .must_be("preparation cwd"),
        fs::canonicalize(session.project()).must_be("project")
    );
    // The overlay view holds every captured non-rule source exactly.
    let overlays = session.overlay_input();
    let root = fs::canonicalize(&session.root).must_be("canonical source");
    assert_eq!(overlays[root.to_string_lossy().as_ref()], SOURCE);
    let config =
        fs::canonicalize(session.project().join("bend-lint.json")).must_be("canonical config");
    assert!(
        overlays.get(config.to_string_lossy().as_ref()).is_none(),
        "trusted rule configuration is never an overlay"
    );
    assert!(session.control.join("stdin-eof").exists());
    let calls = session.calls();
    session.save();
    session.wait_calls(calls + 1);
    session.lint();
    assert_eq!(
        session.prepares(),
        1,
        "a saved check reuses the completed preparation"
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("saved source"),
        SOURCE
    );
    session.finish();
}

#[test]
fn preparation_is_cached_across_typing_and_saving_but_not_configuration() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.lint();
    assert_eq!(session.prepares(), 1);
    let calls = session.calls();
    session.change(2, "def main(x: U32) -> U32:\n  x\n");
    session.wait_calls(calls + 1);
    assert_eq!(
        session.prepares(),
        1,
        "typing reuses the prepared analyzer and rules"
    );
    let calls = session.calls();
    session.save();
    session.wait_calls(calls + 1);
    assert_eq!(session.prepares(), 1, "saving reuses the preparation");
    // A lint configuration change is a new preparation key; the next check
    // prepares again before it runs.
    session.configure(true, false, 20_000);
    let calls = session.calls();
    session.save();
    session.wait_calls(calls + 1);
    assert_eq!(
        session.prepares(),
        2,
        "a changed lint configuration prepares once more"
    );
    session.finish();
}

#[test]
fn preparation_failures_are_reported_without_running_a_check() {
    for (toggle, expected) in [
        ("prepare-fail", "fixture prepare failure"),
        ("prepare-no-report", "did not write its report"),
        ("prepare-stdout", "unexpected stdout"),
    ] {
        let mut session = Session::new(SOURCE, &[]);
        fs::write(session.control.join(toggle), "").must_be("enable preparation failure");
        session.standard_response();
        session.configure(true, false, 10_000);
        session.open(SOURCE);
        let failure = session.failure();
        assert!(
            failure["params"]["message"]
                .as_str()
                .must_be("failure message")
                .contains(expected),
            "{toggle}: {failure}"
        );
        assert_eq!(session.prepares(), 1, "{toggle}");
        assert_eq!(session.calls(), 0, "no check runs after {toggle}");
        let status = session.status();
        assert_eq!(status["state"], "failed", "{toggle}: {status}");
        assert!(
            status["reason"]
                .as_str()
                .must_be("failure reason")
                .contains(expected),
            "{toggle}: {status}"
        );
        session.no_lint();
        session.finish();
    }
}

#[test]
fn an_old_preparation_report_is_rejected_before_any_check() {
    let mut session = Session::new(SOURCE, &[]);
    let report = json!({
        "bendLint":"0.1.1",
        "notes":[],
        "runtime":"bun 1.4.3",
        "platform":"fixture",
        "backend":"native",
        "program":"/fixture/rules.js",
        "cache":{"dir":"/fixture/cache","built":true,"checks":{"cached":0,"checked":0}},
        "timings":{"setupMs":1,"checkerMs":0,"runMs":2},
        "exitCode":0
    });
    fs::write(
        session.control.join("prepare-report.json"),
        serde_json::to_vec(&report).must_be("serialize old report"),
    )
    .must_be("write old preparation report");
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let failure = session.failure();
    assert!(
        failure["params"]["message"]
            .as_str()
            .must_be("version rejection")
            .contains("unsupported bend-lint 0.1.1"),
        "the 0.2.1 contract gate is explicit: {failure}"
    );
    assert_eq!(session.calls(), 0, "no disk-only fallback check");
    session.no_lint();
    session.finish();
}

#[test]
fn cached_preparation_failure_is_retried_only_by_an_explicit_run() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("prepare-fail"), "").must_be("fail preparation");
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.failure();
    assert_eq!(session.prepares(), 1);
    assert_eq!(session.calls(), 0);
    fs::remove_file(session.control.join("prepare-fail")).must_be("allow preparation retry");
    // Ordinary typing is not a retry gesture: the cached failure is reused.
    session.change(2, "def main(x: U32) -> U32:\n  x\n");
    session.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":session.document}}),
    );
    thread::sleep(QUIET);
    assert_eq!(session.prepares(), 1, "typing does not retry preparation");
    assert_eq!(session.calls(), 0);
    assert_eq!(session.status()["state"], "failed");
    // The explicit run is the documented retry gesture.
    session.standard_response();
    let run = session.client.request_response(
        "workspace/executeCommand",
        json!({"command":"bend2.lint.run","arguments":[{"uri":session.document}]}),
    );
    assert!(run.get("error").is_none(), "{run}");
    session.lint();
    assert_eq!(session.prepares(), 2);
    assert_eq!(session.calls(), 1);
    assert_eq!(session.status()["state"], "ready");
    session.finish();
}

#[test]
fn shutdown_reaps_a_blocked_preparation_without_running_a_check() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("prepare-block"), "").must_be("block preparation");
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let address = session.wait_file("prepare-address");
    assert_eq!(session.prepares(), 1);
    assert_eq!(session.calls(), 0);
    let status = session.status();
    assert_eq!(status["state"], "preparing", "{status}");
    let calls = session.calls();
    session.client.finish();
    socket_closed(&address);
    assert_eq!(calls, 0);
    drop(session.directory);
}

#[test]
fn structured_project_rejection_never_becomes_a_source_diagnostic() {
    let mut session = Session::new(SOURCE, &[]);
    let rejection = json!({
        "ok":false,
        "errors":[{
            "path":session.project().join("bend-lint.json"),
            "message":"bend-lint: bend-lint.json: language/unneeded_plus is not a rule"
        }]
    });
    fs::write(
        session.control.join("response.json"),
        serde_json::to_vec(&rejection).must_be("serialize structured rejection"),
    )
    .must_be("write structured rejection");
    fs::write(session.control.join("exit-code"), "2").must_be("rejection exit");
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let failure = session.failure();
    let message = failure["params"]["message"].as_str().must_be("rejection message");
    assert!(
        message.contains("bend-lint rejected the project")
            && message.contains("language/unneeded_plus is not a rule"),
        "the upstream structured error is attributable: {failure}"
    );
    session.no_lint();
    assert_eq!(session.status()["state"], "failed");
    session.finish();
}

#[test]
fn unsaved_changes_are_analyzed_from_overlays_and_never_from_old_disk_text() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.lint();
    let edited = "# edited\ndef main(+x: U32) -> U32:\n  x\n";
    session.response(
        vec![finding(
            &session.root,
            "language/unneeded_plus",
            range(1, 10, 11),
        )],
        vec![],
        true,
    );
    let calls = session.calls();
    session.change(2, edited);
    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && message["params"]["version"] == 2
                && has_lint(message)
        })
        .must_be("the unsaved buffer is checked without a save");
    assert_eq!(packet["params"]["version"], 2);
    session.wait_calls(calls + 1);
    let root = fs::canonicalize(&session.root).must_be("canonical source");
    assert_eq!(
        session.overlay_input()[root.to_string_lossy().as_ref()],
        edited,
        "the analyzer receives the unsaved text, not the old disk copy"
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("unchanged disk"),
        SOURCE
    );
    // A save notification schedules a check immediately but must still send the
    // editor buffer, even while the disk copy is stale.
    let calls = session.calls();
    session.save();
    session.wait_calls(calls + 1);
    let root = fs::canonicalize(&session.root).must_be("canonical source");
    assert_eq!(
        session.overlay_input()[root.to_string_lossy().as_ref()],
        edited
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("still unchanged disk"),
        SOURCE
    );
    session.finish();
}

#[test]
fn compiler_and_lint_publish_as_one_union_and_disabling_only_clears_lint() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR)
        .must_be("compiler error fixture");
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.compiler(1);
    session.save();
    let packet = session.lint();
    assert!(
        packet["params"]["diagnostics"]
            .as_array()
            .must_be("merged diagnostics")
            .iter()
            .any(|item| item["message"] == "selected compiler rejection"
                && item["source"] != "bend-lint")
    );
    session.configure(false, false, 10_000);
    let cleared = session.compiler(1);
    assert!(!has_lint(&cleared));
    session.no_lint();
    session.finish();
}

#[test]
fn delayed_lint_is_discarded_after_edit_and_requests_stay_responsive() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.block();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let address = session.wait_file("address");
    session.change(2, "def main(x: U32) -> U32:\n  x\n");
    let response = session.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":session.document}}),
    );
    assert!(response["result"].is_array());
    session.release();
    socket_closed(&address);
    let fresh = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && message["params"]["version"] == 2
                && has_lint(message)
        })
        .must_be("the edited revision is checked after the superseded one is discarded");
    assert_eq!(fresh["params"]["version"], 2);
    assert!(
        session
            .client
            .receive_matching(Duration::from_millis(250), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == session.document
                    && message["params"]["version"] == 1
                    && has_lint(message)
            })
            .is_none(),
        "findings for the superseded revision must never publish"
    );
    session.finish();
}

#[test]
fn unsaved_imported_dependency_is_analyzed_without_saving() {
    let source = "import dep.bend as Dep\ndef main: U32\n  Dep.value\n";
    let saved_dependency = "def value: U32\n  1\n";
    let dirty_dependency = "def value: U32\n  2\n";
    let mut session = Session::new(source, &[("dep.bend", saved_dependency)]);
    session.response(
        vec![finding(&session.root, "layout/imports", range(0, 0, 6))],
        vec![],
        true,
    );
    session.configure(true, false, 10_000);
    session.open(source);
    let dependency_path = session.project().join("dep.bend");
    let dependency = uri(&dependency_path);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
        "uri":dependency,"languageId":"bend","version":1,"text":dirty_dependency}}),
    );
    session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .must_be("the root is checked with its unsaved imported dependency");
    // Every captured source is in the immutable overlay view with its exact text.
    let overlays = session.overlay_input();
    let root = fs::canonicalize(&session.root).must_be("canonical source");
    let captured_dependency = fs::canonicalize(&dependency_path).must_be("canonical dependency");
    assert_eq!(overlays[root.to_string_lossy().as_ref()], source);
    assert_eq!(
        overlays[captured_dependency.to_string_lossy().as_ref()],
        dirty_dependency
    );
    assert_eq!(
        fs::read_to_string(&dependency_path).must_be("unsaved dependency stays on disk"),
        saved_dependency
    );
    assert!(session.calls() >= 1);
    session.finish();
}

#[test]
fn dependency_edit_discards_inflight_root_findings_and_rechecks() {
    let source = "import dep.bend as Dep\ndef main: U32\n  Dep.value\n";
    let dependency_source = "def value: U32\n  1\n";
    let mut session = Session::new(source, &[("dep.bend", dependency_source)]);
    session.response(
        vec![finding(&session.root, "stale/result", range(0, 0, 6))],
        vec![],
        true,
    );
    session.block();
    session.configure(true, false, 10_000);
    session.open(source);
    let dependency = uri(&session.project().join("dep.bend"));
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
        "uri":dependency,"languageId":"bend","version":1,"text":dependency_source}}),
    );
    let address = session.wait_file("address");
    // The blocked invocation already captured the stale response. The edit
    // invalidates that epoch and schedules a fresh check with the new inputs.
    session.client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":dependency,"version":2},
        "contentChanges":[{"text":"def value: U32\n  2\n"}]}),
    );
    session.response(
        vec![finding(&session.root, "fresh/result", range(0, 1, 6))],
        vec![],
        true,
    );
    session.release();
    socket_closed(&address);
    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .must_be("the edited dependency causes a fresh root check");
    let codes: Vec<_> = packet["params"]["diagnostics"]
        .as_array()
        .must_be("fresh root diagnostics")
        .iter()
        .filter_map(|diagnostic| diagnostic["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"fresh/result"),
        "a completed check over the new inputs published: {codes:?}"
    );
    assert!(
        !codes.contains(&"stale/result"),
        "findings from the superseded inputs must never publish: {codes:?}"
    );
    session.finish();
}

#[test]
fn watched_dependency_change_invalidates_inflight_root_findings() {
    let source = "import dep.bend as Dep\ndef main: U32\n  Dep.value\n";
    let mut session = Session::new(source, &[("dep.bend", "def value: U32\n  1\n")]);
    session.response(
        vec![finding(&session.root, "stale/result", range(0, 0, 6))],
        vec![],
        true,
    );
    session.block();
    session.configure(true, false, 10_000);
    session.open(source);
    session.save();
    let address = session.wait_file("address");
    let dependency = session.project().join("dep.bend");
    fs::write(&dependency, "def value: U32\n  2\n").must_be("external dependency edit");
    session.client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&dependency),"type":2}]}),
    );
    session.release();
    socket_closed(&address);
    // A watched change invalidates finished/in-flight results but does not
    // schedule a check by itself: an unrelated completion must stay hidden.
    session.no_lint();
    session.finish();
}

#[test]
fn superseding_preparation_keeps_the_newest_progress_token_until_completion() {
    let mut session = Session::with_server_capabilities(
        SOURCE,
        &[],
        json!({"window":{"workDoneProgress":true}}),
    );
    fs::write(session.control.join("prepare-block"), "").must_be("block preparation");
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let first_create = session.acknowledge_progress_create();
    let first = first_create["params"]["token"].clone();
    session.progress("begin", &first);
    let prepare_address = session.wait_file("prepare-address");
    // An immediate save supersedes the preparation-blocked run.
    session.save();
    let second_create = session.acknowledge_progress_create();
    let second = second_create["params"]["token"].clone();
    assert_ne!(first, second, "each run owns a distinct progress token");
    session.progress("begin", &second);
    let premature = session.client.receive_matching(QUIET, |message| {
        message["method"] == "$/progress"
            && message["params"]["token"] == second
            && message["params"]["value"]["kind"] == "end"
    });
    assert!(
        premature.is_none(),
        "the replacement token must not end before its check completes: {premature:?}"
    );
    session.progress("end", &first);
    fs::write(session.control.join("prepare-release"), "").must_be("release preparation");
    session.lint();
    session.progress("end", &second);
    assert_eq!(session.status()["state"], "ready");
    socket_closed(&prepare_address);
    session.finish();
}

#[test]
fn suppressed_findings_stay_hidden_and_utf16_positions_are_not_reencoded() {
    let source = "# 😀x\ndef main(+x: U32) -> U32:\n  x\n";
    let mut session = Session::new(source, &[]);
    session.response(
        vec![finding(&session.root, "visible", range(0, 4, 5))],
        vec![finding(&session.root, "suppressed", range(0, 4, 5))],
        true,
    );
    session.configure(true, false, 10_000);
    session.open(source);
    session.save();
    let packet = session.lint();
    let items: Vec<_> = packet["params"]["diagnostics"]
        .as_array()
        .must_be("diagnostics")
        .iter()
        .filter(|item| item["source"] == "bend-lint")
        .collect();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["code"], "visible");
    assert_eq!(items[0]["range"], range(0, 4, 5));
    session.finish();
}

#[test]
fn lint_error_exit_one_is_a_valid_report_not_a_provider_failure() {
    let mut session = Session::new(SOURCE, &[]);
    let mut item = finding(&session.root, "test/error", range(0, 9, 10));
    item["severity"] = json!("error");
    session.response(vec![item], vec![], false);
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let packet = session.lint();
    assert!(
        packet["params"]["diagnostics"]
            .as_array()
            .must_be("diagnostics")
            .iter()
            .any(|item| item["source"] == "bend-lint" && item["severity"] == 1)
    );
    session.finish();
}

#[test]
fn malformed_output_and_process_failure_preserve_selected_compiler_errors() {
    for (output, status) in [
        ("not json", "0"),
        ("{}", "2"),
        ("{\"ok\":true,\"findings\":[],\"suppressed\":[],\"skipped\":[]}", "1"),
    ] {
        let mut session = Session::new(SOURCE, &[]);
        fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR)
            .must_be("compiler error");
        fs::write(session.control.join("response.json"), output).must_be("invalid response");
        fs::write(session.control.join("exit-code"), status).must_be("failing exit");
        session.configure(true, false, 10_000);
        session.open(SOURCE);
        session.compiler(1);
        session.save();
        session.failure();
        session.no_lint();
        session.change(2, SOURCE);
        let packet = session.compiler(2);
        assert!(!has_lint(&packet));
        session.finish();
    }
}

#[test]
fn timeout_kills_analyzer_and_its_descendant_without_blocking_compiler() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR).must_be("compiler error");
    fs::write(session.control.join("spawn-descendant"), "").must_be("enable descendant fixture");
    session.block();
    session.configure(true, false, 500);
    session.open(SOURCE);
    session.compiler(1);
    session.save();
    let address = session.wait_file("address");
    let descendant = session.wait_file("descendant-address");
    session.failure();
    socket_closed(&address);
    socket_closed(&descendant);
    session.change(2, SOURCE);
    session.compiler(2);
    session.no_lint();
    session.finish();
}

#[test]
fn shutdown_reaps_analyzer_process_tree() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("spawn-descendant"), "").must_be("enable descendant");
    session.block();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let address = session.wait_file("address");
    let descendant = session.wait_file("descendant-address");
    session.finish();
    socket_closed(&address);
    socket_closed(&descendant);
}

#[test]
fn close_and_reopen_cannot_accept_previous_save_completion() {
    let mut session = Session::new(SOURCE, &[]);
    session.response(
        vec![finding(&session.root, "stale/result", range(0, 0, 6))],
        vec![],
        true,
    );
    session.block();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let address = session.wait_file("address");
    session.client.notify(
        "textDocument/didClose",
        json!({"textDocument":{"uri":session.document}}),
    );
    session.open(SOURCE);
    // The reopened document starts a fresh epoch; only it may publish.
    session.response(
        vec![finding(&session.root, "fresh/result", range(0, 1, 6))],
        vec![],
        true,
    );
    session.release();
    socket_closed(&address);
    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .must_be("the reopened document is checked");
    let codes: Vec<_> = packet["params"]["diagnostics"]
        .as_array()
        .must_be("reopened diagnostics")
        .iter()
        .filter_map(|diagnostic| diagnostic["code"].as_str())
        .collect();
    assert!(codes.contains(&"fresh/result"), "{codes:?}");
    assert!(
        !codes.contains(&"stale/result"),
        "the closed document's completion must not publish: {codes:?}"
    );
    session.finish();
}

#[test]
fn configuration_change_rejects_pending_results() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.block();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let address = session.wait_file("address");
    session.configure(false, false, 10_000);
    session.release();
    socket_closed(&address);
    session.no_lint();
    session.finish();
}

#[test]
fn dangerous_fixes_require_optin_explicit_request_and_versioned_user_edit() {
    let mut session = Session::new(SOURCE, &[]);
    let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
    item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![item], vec![], true);
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.save();
    let packet = session.lint();
    let diagnostics = packet["params"]["diagnostics"].clone();
    assert!(
        session.actions(diagnostics.clone(), 2).is_empty(),
        "automatic requests must not offer dangerous fixes"
    );
    let actions = session.actions(diagnostics, 1);
    assert_eq!(actions.len(), 1);
    let action = &actions[0];
    assert!(
        action["title"]
            .as_str()
            .must_be("action title")
            .to_ascii_lowercase()
            .contains("dangerous")
    );
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    assert_ne!(action["isPreferred"], true);
    assert!(
        action["command"].is_null(),
        "never execute automatic fix commands"
    );
    assert_eq!(
        action["edit"]["documentChanges"],
        json!([{
            "textDocument":{"uri":session.document,"version":1},
            "edits":[{"range":range(0, 9, 10),"newText":""}]
        }])
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("unchanged disk"),
        SOURCE
    );
    session.finish();
}

#[test]
fn dangerous_fixes_are_absent_without_separate_code_action_consent() {
    let mut session = Session::new(SOURCE, &[]);
    let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
    item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![item], vec![], true);
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    assert!(session.actions(diagnostics, 1).is_empty());
    session.finish();
}

#[test]
fn previously_reported_fix_is_refused_after_unsaved_edit_or_external_disk_change() {
    for external in [false, true] {
        let mut session = Session::new(SOURCE, &[]);
        let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
        item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
        session.response(vec![item], vec![], true);
        session.configure(true, true, 10_000);
        session.open(SOURCE);
        session.save();
        let diagnostics = session.lint()["params"]["diagnostics"].clone();
        if external {
            fs::write(&session.root, "# external\n")
                .must_be("external disk edit without notification");
        } else {
            session.change(2, "# unsaved\n");
        }
        assert!(session.actions(diagnostics, 1).is_empty());
        session.finish();
    }
}

#[test]
fn conflicting_or_unverifiable_multifile_fix_is_not_offered() {
    for invalid in [
        "overlap",
        "closed-target",
        "missing-target",
        "surrogate-boundary",
    ] {
        let mut session = Session::new(SOURCE, &[("dep.bend", "# 😀x\n")]);
        let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
        let mut edits = vec![json!({"path":session.root,"range":range(0, 9, 10),"text":""})];
        match invalid {
            "overlap" => edits.push(json!({"path":session.root,"range":range(0, 9, 11),"text":"x"})),
            "closed-target" | "missing-target" => edits.push(json!({
                "path":session.project().join(if invalid == "closed-target" { "dep.bend" } else { "missing.bend" }),
                "range":range(0, 0, 0),"text":"# changed\n"})),
            "surrogate-boundary" => {
                let dependency = uri(&session.project().join("dep.bend"));
                session.client.notify("textDocument/didOpen", json!({"textDocument":{
                    "uri":dependency,"languageId":"bend","version":1,"text":"# 😀x\n"}}));
                edits.push(json!({"path":session.project().join("dep.bend"),"range":range(0, 3, 4),"text":""}));
            }
            _ => unreachable!("enumerated invalid edit case"),
        }
        item["fixes"] = json!([{"title":"Remove +","applicability":"dangerous","edits":edits}]);
        session.response(vec![item], vec![], true);
        session.configure(true, true, 10_000);
        session.open(SOURCE);
        session.save();
        let diagnostics = session.lint()["params"]["diagnostics"].clone();
        assert!(
            session.actions(diagnostics, 1).is_empty(),
            "reject {invalid} as a whole, not a partial edit"
        );
        session.finish();
    }
}

#[test]
fn later_compiler_completion_keeps_already_published_lint_findings() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR).must_be("compiler error");
    fs::write(session.control.join("compiler-block"), "").must_be("delay compiler");
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.wait_file("compiler-address");
    session.save();
    session.lint();
    fs::write(session.control.join("compiler-release"), "").must_be("finish compiler");
    let packet = session.compiler(1);
    assert!(
        has_lint(&packet),
        "a late compiler must not overwrite the lint slot"
    );
    session.finish();
}

#[test]
fn imported_compiler_and_lint_findings_coexist_and_disable_preserves_imported_error() {
    let source = "import dep.bend as Dep\ndef main: U32\n  Dep.value\n";
    let mut session = Session::new(source, &[("dep.bend", "def value: U32\n  1\n")]);
    let dependency = session.project().join("dep.bend");
    let dependency_uri = uri(&dependency);
    let errors = "Error:\nselected root rejection\nLocation: main\n2>| def main: U32\n  |     ^^^^\n3 |   Dep.value\nError:\nselected imported rejection\nLocation: Dep.value\n1>| def value: U32\n  |     ^^^^^\n2 |   1\n";
    fs::write(session.control.join("compiler.stderr"), errors).must_be("imported compiler error");
    session.response(
        vec![
            finding(&session.root, "root/lint", range(1, 4, 8)),
            finding(&dependency, "dependency/lint", range(0, 4, 9)),
        ],
        vec![],
        true,
    );
    session.configure(true, false, 10_000);
    session.open(source);
    let baseline = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == dependency_uri
                && message["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item["message"] == "selected imported rejection")
                    })
        })
        .must_be("selected imported diagnostic before lint");
    session.save();
    session.lint();
    let merged = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["params"]["uri"] == dependency_uri && has_lint(message)
        })
        .must_be("imported compiler and lint union");
    let selected: Vec<_> = merged["params"]["diagnostics"]
        .as_array()
        .must_be("imported union")
        .iter()
        .filter(|item| item["source"] != "bend-lint")
        .cloned()
        .collect();
    assert_eq!(json!(selected), baseline["params"]["diagnostics"]);
    session.configure(false, false, 10_000);
    let cleared = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == dependency_uri
                && message["params"]["diagnostics"] == baseline["params"]["diagnostics"]
        })
        .must_be("disable clears only imported lint findings");
    assert!(!has_lint(&cleared));
    session.finish();
}

#[test]
fn editing_saved_document_refreshes_lint_from_the_overlay() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.lint();
    // The unsaved edit removes the only finding; that is a real check result,
    // not an interim clear while waiting for a save.
    session.response(vec![], vec![], true);
    let edited = "def main(x: U32) -> U32:\n  x\n";
    session.change(2, edited);
    session.wait_calls(2);
    let cleared = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && message["params"]["version"] == 2
                && !has_lint(message)
        })
        .must_be("the unsaved edit is checked and its finding disappears");
    assert!(!has_lint(&cleared));
    let root = fs::canonicalize(&session.root).must_be("canonical source");
    assert_eq!(session.overlay_input()[root.to_string_lossy().as_ref()], edited);
    assert_eq!(
        fs::read_to_string(&session.root).must_be("unchanged disk"),
        SOURCE
    );
    session.finish();
}

#[test]
fn config_file_change_invalidates_pending_project_results() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.block();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let address = session.wait_file("address");
    let config = session.project().join("bend-lint.json");
    fs::write(&config, "{\"rules\":{\"language/unneeded_plus\":\"off\"}}")
        .must_be("change project rules");
    session.client.notify(
        "workspace/didChangeWatchedFiles",
        json!({
            "changes":[{"uri":uri(&config),"type":2}]
        }),
    );
    session.release();
    socket_closed(&address);
    session.no_lint();
    session.finish();
}

#[test]
fn invalid_ranges_and_outside_paths_never_become_editor_findings() {
    for invalid in ["surrogate", "reversed", "past-end", "outside"] {
        let source = "# 😀x\ndef main(+x: U32) -> U32:\n  x\n";
        let mut session = Session::new(source, &[]);
        let (path, span) = match invalid {
            "surrogate" => (session.root.clone(), range(0, 3, 4)),
            "reversed" => (session.root.clone(), range(0, 5, 4)),
            "past-end" => (session.root.clone(), range(99, 0, 1)),
            "outside" => {
                let outside = session.directory.path().join("outside.bend");
                fs::write(&outside, source).must_be("outside project source");
                (outside, range(0, 4, 5))
            }
            _ => unreachable!("enumerated invalid finding case"),
        };
        session.response(vec![finding(&path, "invalid/location", span)], vec![], true);
        session.configure(true, false, 10_000);
        session.open(source);
        session.save();
        session.wait_file("started");
        socket_closed(&session.wait_file("address"));
        session.no_lint();
        session.finish();
    }
}

#[test]
fn missing_analyzer_is_visible_without_erasing_compiler_diagnostics() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR).must_be("compiler error");
    let missing = session.directory.path().join("missing analyzer");
    session.client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{
            "bend2-lsp":{"compilerPath":executable()},
            "bend":{"lint":{"enabled":true,"path":missing,"timeoutMs":10_000}}
        }}),
    );
    session.open(SOURCE);
    session.compiler(1);
    session.save();
    session.failure();
    session.no_lint();
    session.change(2, SOURCE);
    session.compiler(2);
    session.finish();
}

#[test]
fn relative_analyzer_path_is_never_resolved_from_project_or_path() {
    let mut session = Session::new(SOURCE, &[]);
    session.client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{
            "bend2-lsp":{"compilerPath":executable()},
            "bend":{"lint":{"enabled":true,"path":"bend-lint","timeoutMs":10_000}}
        }}),
    );
    session.open(SOURCE);
    session.save();
    session.no_lint();
    assert!(!session.control.join("calls").exists());
    session.finish();
}

#[test]
fn valid_multifile_dangerous_action_versions_every_open_saved_target() {
    let dependency_source = "def value: U32\n  1\n";
    let mut session = Session::new(SOURCE, &[("dep.bend", dependency_source)]);
    let dependency = session.project().join("dep.bend");
    let dependency_uri = uri(&dependency);
    let mut item = finding(&session.root, "custom/multifile", range(0, 9, 10));
    item["fixes"] = json!([{"title":"Change both files","applicability":"dangerous","edits":[
        {"path":session.root,"range":range(0, 9, 10),"text":""},
        {"path":dependency,"range":range(1, 2, 3),"text":"2"}
    ]}]);
    session.response(vec![item], vec![], true);
    session.configure(true, true, 10_000);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":dependency_uri,"languageId":"bend","version":7,"text":dependency_source
        }}),
    );
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    let actions = session.actions(diagnostics.clone(), 1);
    assert_eq!(actions.len(), 1);
    let edits = actions[0]["edit"]["documentChanges"]
        .as_array()
        .must_be("multifile changes");
    assert_eq!(edits.len(), 2);
    let root_edit = edits
        .iter()
        .find(|edit| edit["textDocument"]["uri"] == session.document)
        .must_be("root edit");
    assert_eq!(root_edit["textDocument"]["version"], 1);
    assert_eq!(
        root_edit["edits"],
        json!([{"range":range(0, 9, 10),"newText":""}])
    );
    let dep_edit = edits
        .iter()
        .find(|edit| edit["textDocument"]["uri"] == dependency_uri)
        .must_be("dependency edit");
    assert_eq!(dep_edit["textDocument"]["version"], 7);
    assert_eq!(
        dep_edit["edits"],
        json!([{"range":range(1, 2, 3),"newText":"2"}])
    );
    session.client.notify(
        "textDocument/didChange",
        json!({"textDocument":{
            "uri":dependency_uri,"version":8},"contentChanges":[{"text":"# unsaved\n"}]
        }),
    );
    assert!(
        session.actions(diagnostics, 1).is_empty(),
        "a dirty target invalidates the entire edit"
    );
    assert_eq!(
        fs::read_to_string(&dependency).must_be("unchanged dependency"),
        dependency_source
    );
    session.finish();
}

#[test]
fn lint_rule_cannot_impersonate_an_automatic_parser_fix() {
    let mut session = Session::new(SOURCE, &[]);
    let mut item = finding(&session.root, "parsing", range(0, 9, 10));
    item["message"] = json!("Unclosed '('");
    session.response(vec![item], vec![], true);
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    let response = session.client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":session.document},
            "range":range(0, 0, 24),
            "context":{"diagnostics":diagnostics,"triggerKind":2,"only":["quickfix"]}
        }),
    );
    assert!(
        response["result"]
            .as_array()
            .is_none_or(|actions| { actions.iter().all(|action| action["title"] != "Insert ')'") }),
        "external lint findings must never synthesize preferred parser edits"
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("unchanged source"),
        SOURCE
    );
    session.finish();
}

#[cfg(unix)]
#[test]
fn retargeting_project_symlink_discards_inflight_findings() {
    use std::os::unix::fs::symlink;

    let first = "def first(x: U32) -> U32:\n  x\n";
    let second = "def second(x: U32) -> U32:\n  x\n";
    let mut session = Session::new(SOURCE, &[("a.bend", first), ("b.bend", second)]);
    let alias = session.project().join("alias.bend");
    let target = session.project().join("b.bend");
    symlink(session.project().join("a.bend"), &alias).must_be("create original source alias");
    session.response(
        vec![finding(&alias, "language/unneeded_plus", range(0, 0, 3))],
        vec![],
        true,
    );
    session.block();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":uri(&target),"languageId":"bend","version":1,"text":second}}),
    );
    session.save();
    let address = session.wait_file("address");
    fs::remove_file(&alias).must_be("remove old source alias");
    symlink(&target, &alias).must_be("retarget source alias without changing either source");
    session.release();
    socket_closed(&address);
    session.no_lint();
    session.finish();
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_symlink_target_keeps_lossless_diagnostics_and_server_responsive() {
    use std::{
        ffi::OsString,
        os::unix::{ffi::OsStringExt, fs::symlink},
    };

    let mut session = Session::new(SOURCE, &[]);
    let target = session
        .directory
        .path()
        .join(OsString::from_vec(b"\xff.bend".to_vec()));
    fs::write(&target, SOURCE).must_be("write non-UTF-8 native source path");
    let alias = session.project().join("alias.bend");
    symlink(&target, &alias).must_be("create UTF-8 alias to native source path");
    session.response(
        vec![finding(&alias, "language/unneeded_plus", range(0, 9, 10))],
        vec![],
        true,
    );
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let target_uri = uri(&target);
    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            has_lint(message) && message["params"]["uri"] == target_uri
        })
        .must_be("native source receives lint at its lossless file URI");
    assert_eq!(
        packet["params"]["diagnostics"][0]["code"],
        "language/unneeded_plus"
    );
    let response = session.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":session.document}}),
    );
    assert_eq!(response["result"][0]["name"], "main");
    session.finish();
}

#[cfg(unix)]
#[test]
fn retargeting_project_directory_symlink_discards_inflight_findings() {
    use std::os::unix::fs::symlink;

    let first = "def first(x: U32) -> U32:\n  x\n";
    let second = "def second(x: U32) -> U32:\n  x\n";
    let mut session = Session::new(SOURCE, &[]);
    let original_directory = session.project().join("a");
    let target_directory = session.project().join("b");
    for (directory, text) in [(&original_directory, first), (&target_directory, second)] {
        fs::create_dir(directory).must_be("create saved source directory");
        fs::write(directory.join("source.bend"), text).must_be("write saved directory source");
    }
    let alias = session.project().join("alias");
    let target = target_directory.join("source.bend");
    symlink(&original_directory, &alias).must_be("create original directory alias");
    session.response(
        vec![finding(
            &alias.join("source.bend"),
            "language/unneeded_plus",
            range(0, 0, 3),
        )],
        vec![],
        true,
    );
    session.block();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":uri(&target),"languageId":"bend","version":1,"text":second}}),
    );
    session.save();
    let address = session.wait_file("address");
    fs::remove_file(&alias).must_be("remove old directory alias");
    symlink(&target_directory, &alias).must_be("retarget directory alias");
    session.release();
    socket_closed(&address);
    session.no_lint();
    session.finish();
}

#[path = "bend_lint_dx/results.rs"]
mod dx_results;

#[path = "bend_lint_dx/status.rs"]
mod dx_status;

#[path = "bend_lint_dx/actions.rs"]
mod dx_actions;
