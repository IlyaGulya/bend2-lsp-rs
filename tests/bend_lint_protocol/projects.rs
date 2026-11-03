use super::lsp_client::LspClient;
use super::*;
use std::{
    env, fs,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

// One server, three independent projects. The fixture analyzer keeps a
// per-target report, so each project can prove which check actually produced a
// packet.
struct Projects {
    directory: TempDir,
    a_uri: String,
    a_source: String,
    b_uri: String,
    b_source: String,
    c_uri: String,
    c_source: String,
    client: LspClient,
}

impl Projects {
    fn start() -> Self {
        let directory = tempdir().must_be("two-project lint workspace");
        let control = directory.path().join("control");
        fs::create_dir(&control).must_be("create analyzer controls");
        let responses = control.join("responses");
        fs::create_dir(&responses).must_be("create per-target responses");
        let a_directory = directory.path().join("project a");
        let b_directory = directory.path().join("project b");
        let c_directory = directory.path().join("project c");
        for project in [&a_directory, &b_directory, &c_directory] {
            fs::create_dir(project).must_be("create lint project");
            fs::write(project.join("bend-lint.json"), "{\"rules\":{}}")
                .must_be("write project configuration");
        }
        let a_path = a_directory.join("a.bend");
        let b_path = b_directory.join("b.bend");
        let c_path = c_directory.join("c.bend");
        let a_source = "def main: U32\n  1\n";
        let b_source = "def main: U32\n  2\n";
        let c_source = "def identity: U32\n  1\n";
        fs::write(&a_path, a_source).must_be("write saved entry a");
        fs::write(&b_path, b_source).must_be("write saved entry b");
        fs::write(&c_path, c_source).must_be("write saved entry c");
        target_response(
            &responses.join("a.bend.json"),
            &[finding(&a_path, "project/a", range(0, 0, 3))],
        );
        let mut b_finding = finding(&b_path, "project/b", range(0, 0, 3));
        b_finding["fixes"] = json!([fix(&b_path, range(0, 0, 3), "3")]);
        target_response(&responses.join("b.bend.json"), &[b_finding]);
        target_response(
            &responses.join("c.bend.json"),
            &[finding(&c_path, "project/c", range(0, 0, 3))],
        );
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
            .current_dir(&a_directory)
            .stderr(Stdio::null())
            .env("PATH", &control)
            .env("BEND_LINT_FIXTURE_CONTROL", &control)
            .env_remove("BEND_LIB");
        let mut client = LspClient::spawn(command);
        client.initialize_with_capabilities(directory.path(), &json!({}));
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{
                "bend2-lsp":{"compilerPath":executable()},
                "bend":{"lint":{"enabled":true,"path":executable(),
                    "dangerousCodeActions":true,"timeoutMs":120_000}}
            }}),
        );
        Self {
            directory,
            a_uri: uri(&a_path),
            a_source: a_source.into(),
            b_uri: uri(&b_path),
            b_source: b_source.into(),
            c_uri: uri(&c_path),
            c_source: c_source.into(),
            client,
        }
    }

    fn open(&mut self, uri: &str, source: &str) {
        self.client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,"languageId":"bend","version":1,"text":source}}),
        );
    }

    fn change(&mut self, uri: &str, version: i32, source: &str) {
        self.client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":version},
                "contentChanges":[{"text":source}]}),
        );
    }

    fn wait_lint(&mut self, uri: &str, version: i32) -> Value {
        let uri = uri.to_owned();
        self.client
            .receive_matching(TIMEOUT, move |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && message["params"]["version"] == version
                    && has_lint(message)
            })
            .must_be("the initial entry receives lint diagnostics")
    }

    // The fixture appends one line per real `bend-lint prepare` invocation.
    fn prepares(&self) -> usize {
        fs::read_to_string(self.directory.path().join("control").join("prepares"))
            .map_or(0, |text| text.lines().count())
    }

    // The fixture appends one line per real analyzer invocation.
    fn calls(&self) -> usize {
        fs::read_to_string(self.directory.path().join("control").join("calls"))
            .map_or(0, |text| text.lines().count())
    }

    fn wait_calls(&self, count: usize) {
        let deadline = Instant::now() + TIMEOUT;
        while self.calls() < count {
            assert!(
                Instant::now() < deadline,
                "the analyzer did not run {count} time(s)"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    // The fixture parks every analyzer invocation until the release file exists.
    fn block(&self) {
        fs::write(self.directory.path().join("control").join("block"), "")
            .must_be("block analyzer completion");
    }

    fn publish_while_typing(&mut self, typing_uri: &str, observing_uri: &str) -> Option<Value> {
        let mut version = 2;
        let mut opened = false;
        let mut published = None;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            self.change(
                typing_uri,
                version,
                &format!("def main: U32\n  {version}\n"),
            );
            version += 1;
            if !opened {
                let source = self.b_source.clone();
                self.open(observing_uri, &source);
                opened = true;
            }
            // Pace the next keystroke (well inside the 250 ms debounce) while
            // draining the reader for the observing project. A zero-timeout
            // poll only checks already-buffered messages and can never observe
            // a packet that arrives later on the channel.
            let observing_uri = observing_uri.to_owned();
            if let Some((message, _)) =
                self.client
                    .receive_matching_timed(Duration::from_millis(30), move |message| {
                        message["method"] == "textDocument/publishDiagnostics"
                            && message["params"]["uri"] == observing_uri
                            && has_lint(message)
                    })
            {
                published = Some(message);
                break;
            }
        }
        published
    }

    fn finish(self) {
        self.client.finish();
        drop(self.directory);
    }
}

fn target_response(path: &Path, findings: &[Value]) {
    let response = json!({"ok":true,"findings":findings,"suppressed":[],"skipped":[]});
    fs::write(
        path,
        serde_json::to_vec(&response).must_be("serialize target-specific response"),
    )
    .must_be("write target-specific response");
}

// An invoked quick-fix request that only accepts the dangerous bend-lint kind.
fn dangerous_code_action(uri: &str) -> Value {
    json!({
        "textDocument":{"uri":uri},
        "range":range(0, 0, 3),
        "context":{"diagnostics":[],"triggerKind":1,
            "only":["quickfix.bend-lint.dangerous"]}
    })
}

// The document version carried by an applicable dangerous edit for `uri`.
fn dangerous_version(actions: &Value, uri: &str) -> Option<i64> {
    for action in actions["result"].as_array()? {
        if action["kind"].as_str() != Some("quickfix.bend-lint.dangerous") {
            continue;
        }
        let Some(changes) = action["edit"]["documentChanges"].as_array() else {
            continue;
        };
        for change in changes {
            if change["textDocument"]["uri"].as_str() == Some(uri) {
                return change["textDocument"]["version"].as_i64();
            }
        }
    }
    None
}

// Regression: a shared debounce let keystrokes in A starve B's queued check.
// B must publish its own fresh diagnostics while A is still typing.
#[test]
fn typing_in_one_project_still_lets_an_independent_project_publish() {
    let mut projects = Projects::start();
    let (a_uri, a_source) = (projects.a_uri.clone(), projects.a_source.clone());
    let b_uri = projects.b_uri.clone();
    projects.open(&a_uri, &a_source);
    projects.wait_lint(&a_uri, 1);
    let packet = projects
        .publish_while_typing(&a_uri, &b_uri)
        .must_be("an independent project must publish while another project keeps typing");
    assert_eq!(packet["params"]["version"], 1);
    let codes: Vec<_> = packet["params"]["diagnostics"]
        .as_array()
        .must_be("independent project diagnostics")
        .iter()
        .filter_map(|diagnostic| diagnostic["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"project/b"),
        "the independent project published its own result: {codes:?}"
    );
    projects.finish();
}

// Preparation belongs to the project that owns it. B preparing between A's
// checks must not replace A's prepared analyzer; the fixture's prepare log is
// the external observation of how many real preparations ran.
#[test]
fn an_accepted_change_reuses_its_projects_preparation_after_another_project_prepares() {
    let mut projects = Projects::start();
    let (a_uri, a_source) = (projects.a_uri.clone(), projects.a_source.clone());
    let (b_uri, b_source) = (projects.b_uri.clone(), projects.b_source.clone());
    projects.open(&a_uri, &a_source);
    projects.wait_lint(&a_uri, 1);
    projects.open(&b_uri, &b_source);
    projects.wait_lint(&b_uri, 1);
    assert_eq!(
        projects.prepares(),
        2,
        "each independent project prepared its analyzer once"
    );
    let a_path = projects.directory.path().join("project a").join("a.bend");
    let responses = projects.directory.path().join("control").join("responses");
    target_response(
        &responses.join("a.bend.json"),
        &[finding(&a_path, "project/a2", range(0, 0, 3))],
    );
    projects.change(&a_uri, 2, "def main: U32\n  3\n");
    let packet = projects.wait_lint(&a_uri, 2);
    let codes: Vec<_> = packet["params"]["diagnostics"]
        .as_array()
        .must_be("refreshed project diagnostics")
        .iter()
        .filter_map(|diagnostic| diagnostic["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"project/a2"),
        "the accepted change ran the analyzer again: {codes:?}"
    );
    assert_eq!(
        projects.prepares(),
        2,
        "A's accepted recheck reuses A's preparation after B prepared its own"
    );
    projects.finish();
}

// Regression: the code-action request held the workspace update read guard
// while it waited for the single shared analyzer permit. A blocked analyzer run
// in one project therefore froze didChange for every project, and other
// requests could not observe an unrelated document's new version until that
// analyzer finished. C's update must commit and be observable while A still
// holds the permit and B's action is outstanding.
#[test]
fn code_action_waiting_for_a_blocked_analyzer_does_not_freeze_unrelated_updates() {
    let mut projects = Projects::start();
    let (a_uri, a_source) = (projects.a_uri.clone(), projects.a_source.clone());
    let (b_uri, b_source) = (projects.b_uri.clone(), projects.b_source.clone());
    let (c_uri, c_source) = (projects.c_uri.clone(), projects.c_source.clone());
    for (uri, source) in [
        (&a_uri, &a_source),
        (&b_uri, &b_source),
        (&c_uri, &c_source),
    ] {
        projects.open(uri, source);
        projects.wait_lint(uri, 1);
    }

    // Precondition: B really offers the opt-in dangerous fix at this request
    // shape, so a later missing action cannot be mistaken for the lock bug.
    let params = dangerous_code_action(&b_uri);
    let offered = projects
        .client
        .request("textDocument/codeAction", params.clone());
    assert_eq!(
        dangerous_version(&offered, &b_uri),
        Some(1),
        "B must offer its versioned dangerous fix while the analyzer is idle: {offered}"
    );

    // A blocked analyzer invocation holds the workspace's single analyzer permit.
    projects.block();
    projects.change(&a_uri, 2, "def main: U32\n  4\n");
    projects.wait_calls(4);

    let action_id = projects
        .client
        .send_request("textDocument/codeAction", params);
    // Causal admission barrier, from the vendored transport contract:
    // vendor/tower-lsp/src/transport.rs reads stdin in order, calls
    // `service.call` per message, and sends those futures into one FIFO tokio
    // channel (read_input); process_server_tasks then runs
    // `server_tasks_rx.buffer_unordered(4)`. futures-util 0.3.34's
    // BufferUnordered first fills its FuturesUnordered from that channel in
    // arrival order and only then polls it, and FuturesUnordered::push enqueues
    // each future on a FIFO ready queue. So the earlier code action is polled
    // (and parks on the analyzer permit) before the later probe can run, and
    // the probe's response therefore proves the action had already entered its
    // wait.
    let probe = projects.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":b_uri}}),
    );
    assert!(probe["result"].is_array(), "{probe}");
    assert!(
        projects
            .client
            .receive_matching_timed(Duration::from_millis(50), |message| {
                message["id"] == action_id
            })
            .is_none(),
        "the code action cannot resolve while the analyzer permit is held"
    );

    // Independent C must commit and publish its new version while B waits.
    projects.change(&c_uri, 2, "def renamed: U32\n  7\n");
    let hover_id = projects.client.send_request(
        "textDocument/hover",
        json!({"textDocument":{"uri":c_uri},
            "position":{"line":0,"character":5}}),
    );
    let hover = projects
        .client
        .receive_matching(TIMEOUT, |message| message["id"] == hover_id)
        .must_be(
            "an unrelated document update must become observable while a code action waits for the blocked analyzer",
        );
    assert!(
        hover["result"]["contents"]["value"]
            .as_str()
            .is_some_and(|text| text.contains("renamed: U32")),
        "C's didChange must commit while A is still blocked: {hover}"
    );
    assert!(
        !projects
            .directory
            .path()
            .join("control")
            .join("release")
            .exists()
            && projects.calls() == 4,
        "the analyzer must still be blocked when C's new version is observed"
    );
    assert!(
        projects
            .client
            .receive_matching_timed(Duration::from_millis(50), |message| {
                message["id"] == action_id
            })
            .is_none(),
        "B's code action must still be outstanding when C's update is observed"
    );

    // Releasing A lets B's waiting action resolve against current inputs.
    fs::write(
        projects.directory.path().join("control").join("release"),
        "",
    )
    .must_be("release the blocked analyzer");
    let resolved = projects
        .client
        .receive_matching(TIMEOUT, |message| message["id"] == action_id)
        .must_be("the waiting code action resolves after the analyzer is released");
    assert_eq!(
        dangerous_version(&resolved, &b_uri),
        Some(1),
        "the waiting action still resolves to the current versioned edit: {resolved}"
    );
    projects.finish();
}
