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

// Verified against cli.bend Object.finding/Object.fix/end at upstream
// 02ef84b38b8c06cbd607e270c4930e633e9dab0f, not an LSP-shaped invented payload.
fn finding(path: &Path, code: &str, span: Value) -> Value {
    json!({"code":code,"severity":"warning","message":"remove unneeded plus",
        "path":path,"range":span,"fixes":[]})
}

fn fix(path: &Path, span: Value, replacement: &str) -> Value {
    json!({"title":"Remove +","applicability":"dangerous",
        "edits":[{"path":path,"range":span,"text":replacement}]})
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
            "{\"ok\":true,\"findings\":[],\"suppressed\":[]}",
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
        client.initialize(&project);
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
        fs::write(
            self.control.join("response.json"),
            serde_json::to_vec(&json!({"ok":ok,"findings":findings,"suppressed":suppressed}))
                .must_be("serialize upstream response"),
        )
        .must_be("write analyzer response");
        fs::write(self.control.join("exit-code"), if ok { "0" } else { "1" })
            .must_be("write upstream exit contract");
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

    fn release(&self) {
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
        self.client.request(
            "textDocument/codeAction",
            json!({
                "textDocument":{"uri":self.document},"range":range(0, 0, 24),
                "context":{"diagnostics":diagnostics,"triggerKind":trigger,
                    "only":["quickfix.bend-lint.dangerous"]}
            }),
        )["result"]
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
fn enabled_save_uses_json_in_nearest_project_without_fixing_disk() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.no_lint();
    assert!(
        !session.control.join("calls").exists(),
        "opening is not saving"
    );
    session.save();
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
    assert!(arguments.lines().any(|arg| arg == "--json"));
    assert!(!arguments.contains("--fix"));
    let cwd = PathBuf::from(session.wait_file("cwd"));
    assert_eq!(
        fs::canonicalize(cwd).must_be("invocation cwd"),
        fs::canonicalize(session.project()).must_be("project")
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("saved source"),
        SOURCE
    );
    session.finish();
}

#[test]
fn changes_and_unsaved_save_notifications_never_analyze_old_disk_text() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let edited = "# edited\ndef main(+x: U32) -> U32:\n  x\n";
    session.change(2, edited);
    session.save();
    session.no_lint();
    assert!(!session.control.join("calls").exists());
    fs::write(&session.root, edited).must_be("editor saves current text");
    session.response(
        vec![finding(&session.root, "language/unneeded_plus", range(1, 9, 10))],
        vec![],
        true,
    );
    session.save();
    assert_eq!(session.lint()["params"]["version"], 2);
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
    session.no_lint();
    session.finish();
}

#[test]
fn unsaved_imported_dependency_prevents_saved_root_analysis() {
    let source = "import dep.bend as Dep\ndef main: U32\n  Dep.value\n";
    let mut session = Session::new(source, &[("dep.bend", "def value: U32\n  1\n")]);
    session.configure(true, false, 10_000);
    session.open(source);
    let dependency = uri(&session.project().join("dep.bend"));
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
        "uri":dependency,"languageId":"bend","version":1,"text":"def value: U32\n  2\n"}}),
    );
    session.save();
    session.no_lint();
    assert!(!session.control.join("calls").exists());
    session.finish();
}

#[test]
fn dependency_edit_invalidates_inflight_root_findings() {
    let source = "import dep.bend as Dep\ndef main: U32\n  Dep.value\n";
    let dependency_source = "def value: U32\n  1\n";
    let mut session = Session::new(source, &[("dep.bend", dependency_source)]);
    session.response(
        vec![finding(&session.root, "layout/imports", range(0, 0, 6))],
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
    session.save();
    let address = session.wait_file("address");
    session.client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":dependency,"version":2},
        "contentChanges":[{"text":"def value: U32\n  2\n"}]}),
    );
    session.release();
    socket_closed(&address);
    session.no_lint();
    session.finish();
}

#[test]
fn watched_dependency_change_invalidates_inflight_root_findings() {
    let source = "import dep.bend as Dep\ndef main: U32\n  Dep.value\n";
    let mut session = Session::new(source, &[("dep.bend", "def value: U32\n  1\n")]);
    session.response(
        vec![finding(&session.root, "layout/imports", range(0, 0, 6))],
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
    session.no_lint();
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
        ("{\"ok\":true,\"findings\":[],\"suppressed\":[]}", "1"),
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
    session.standard_response();
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
    session.release();
    socket_closed(&address);
    session.no_lint();
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
fn editing_saved_document_clears_published_lint_until_next_real_save() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    session.lint();
    session.change(2, "def main(x: U32) -> U32:\n  x\n");
    let cleared = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && message["params"]["version"] == 2
                && !has_lint(message)
        })
        .must_be("unsaved edit immediately removes disk diagnostics");
    assert!(!has_lint(&cleared));
    session.no_lint();
    assert_eq!(
        session.wait_file("calls").lines().count(),
        1,
        "no lint on change"
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
