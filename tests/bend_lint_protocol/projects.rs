use super::lsp_client::LspClient;
use super::*;
use std::{
    env, fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// One server, two independent projects. The fixture analyzer keeps a per-target
// report, so each project can prove which check actually produced a packet.
struct Projects {
    directory: TempDir,
    a_uri: String,
    a_source: String,
    b_uri: String,
    b_source: String,
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
        for project in [&a_directory, &b_directory] {
            fs::create_dir(project).must_be("create lint project");
            fs::write(project.join("bend-lint.json"), "{\"rules\":{}}")
                .must_be("write project configuration");
        }
        let a_path = a_directory.join("a.bend");
        let b_path = b_directory.join("b.bend");
        let a_source = "def main: U32\n  1\n";
        let b_source = "def main: U32\n  2\n";
        fs::write(&a_path, a_source).must_be("write saved entry a");
        fs::write(&b_path, b_source).must_be("write saved entry b");
        target_response(
            &responses.join("a.bend.json"),
            &[finding(&a_path, "project/a", range(0, 0, 3))],
        );
        target_response(
            &responses.join("b.bend.json"),
            &[finding(&b_path, "project/b", range(0, 0, 3))],
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
                    "dangerousCodeActions":false,"timeoutMs":10_000}}
            }}),
        );
        Self {
            directory,
            a_uri: uri(&a_path),
            a_source: a_source.into(),
            b_uri: uri(&b_path),
            b_source: b_source.into(),
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
