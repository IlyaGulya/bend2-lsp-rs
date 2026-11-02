use super::lsp_client::LspClient;
use super::*;

const RULE_PROGRAM: &str = "import ../../shared/helper.bend as Helper\ndef rules = Helper.value\n";
const RULE_HELPER: &str = "def value = 1\n";
const CHANGED_RULE_HELPER: &str = "def value = 2\n";
const INNER_SOURCE: &str = "def extra: U32\n  7\n";

fn target_response(control: &Path, entry: &Path, code: &str) {
    let name = entry
        .file_name()
        .must_be("entry file name")
        .to_str()
        .must_be("UTF-8 entry name")
        .to_owned();
    let responses = control.join("responses");
    fs::create_dir_all(&responses).must_be("create per-target responses");
    let response = json!({
        "ok":true,
        "findings":[finding(entry, code, range(0, 0, 3))],
        "suppressed":[],
        "skipped":[]
    });
    fs::write(
        responses.join(format!("{name}.json")),
        serde_json::to_vec(&response).must_be("serialize target response"),
    )
    .must_be("write target response");
}

fn entry_state(client: &mut LspClient, uri: &str) -> String {
    client.request(
        "workspace/executeCommand",
        json!({"command":"bend2.lint.status","arguments":[{"uri":uri}]}),
    )["result"]["state"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn await_state(session: &mut Session, uri: &str, expected: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while entry_state(&mut session.client, uri) != expected {
        assert!(
            Instant::now() < deadline,
            "the entry reaches {expected}: {}",
            entry_state(&mut session.client, uri)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

// Project membership is discovered from disk, so one watcher notification must
// keep an already-open entry's ownership coherent across creation, nesting and
// deletion: the entry moves to the project that now owns it, is checked there,
// and when no project is left it stops being checked and its stale findings are
// withdrawn instead of lingering under a retired membership.
#[test]
fn project_configuration_membership_tracks_creation_nesting_and_deletion() {
    let mut session = Session::new(SOURCE, &[]);
    let inner_directory = session.project().join("inner");
    fs::create_dir(&inner_directory).must_be("create the nested directory");
    let inner_entry = inner_directory.join("main2.bend");
    fs::write(&inner_entry, INNER_SOURCE).must_be("write the nested entry");
    let main_config = session.project().join("bend-lint.json");
    let inner_config = inner_directory.join("bend-lint.json");
    let main_entry = session.root.clone();
    target_response(&session.control, &main_entry, "project/main");
    target_response(&session.control, &inner_entry, "project/outer");
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let inner_uri = uri(&inner_entry);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":inner_uri,"languageId":"bend","version":1,"text":INNER_SOURCE}}),
    );
    let main_uri = session.document.clone();
    session
        .client
        .receive_matching(TIMEOUT, |message| has_lint_code(message, "project/main"))
        .must_be("the outer entry is checked under the outer project");
    let published = inner_uri.clone();
    session
        .client
        .receive_matching(TIMEOUT, move |message| {
            message["params"]["uri"] == published && has_lint_code(message, "project/outer")
        })
        .must_be("the nested entry is checked under the outer project");

    // Creating a nested configuration moves only the nested entry, which must
    // be checked under its new project without losing its open view.
    target_response(&session.control, &inner_entry, "project/nested");
    fs::write(&inner_config, "{\"rules\":{}}").must_be("create the nested configuration");
    session.client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&inner_config),"type":1}]}),
    );
    let published = inner_uri.clone();
    session
        .client
        .receive_matching(TIMEOUT, move |message| {
            message["params"]["uri"] == published && has_lint_code(message, "project/nested")
        })
        .must_be("the nested entry is rechecked under the created nested project");
    await_state(&mut session, &main_uri, "ready");
    await_state(&mut session, &inner_uri, "ready");

    // Deleting that configuration returns the entry to the outer project.
    target_response(&session.control, &inner_entry, "project/restored");
    fs::remove_file(&inner_config).must_be("delete the nested configuration");
    session.client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&inner_config),"type":3}]}),
    );
    let published = inner_uri.clone();
    session
        .client
        .receive_matching(TIMEOUT, move |message| {
            message["params"]["uri"] == published && has_lint_code(message, "project/restored")
        })
        .must_be("the nested entry is rechecked under the outer project again");
    await_state(&mut session, &main_uri, "ready");
    await_state(&mut session, &inner_uri, "ready");

    // With no configuration left there is no project to check against: both
    // open entries report the missing configuration and their retired findings
    // are withdrawn instead of being served from the old membership.
    let calls = session.calls();
    fs::remove_file(&main_config).must_be("delete the outer configuration");
    session.client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&main_config),"type":3}]}),
    );
    let withdrawn = main_uri.clone();
    session
        .client
        .receive_matching(TIMEOUT, move |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == withdrawn
                && !has_lint(message)
        })
        .must_be("the retired outer findings are withdrawn");
    let withdrawn = inner_uri.clone();
    session
        .client
        .receive_matching(TIMEOUT, move |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == withdrawn
                && !has_lint(message)
        })
        .must_be("the retired nested findings are withdrawn");
    await_state(&mut session, &main_uri, "skipped");
    await_state(&mut session, &inner_uri, "skipped");
    assert_eq!(
        session.calls(),
        calls,
        "an entry without a project is never handed to the analyzer"
    );
    session.finish();
}

// Regression: the first check captures the saved rule program before any
// result exists, so nothing but the live captured view names this helper, and
// the workspace has no reverse import edge for it either. A watcher event for
// the helper during the first preparation must still reach the project, fence
// the in-flight check, recapture the saved rule input and publish findings for
// the bytes that are on disk now instead of leaving the entry skipped.
#[test]
fn changing_an_external_rule_helper_during_the_first_preparation_rechecks_the_entry() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    let workspace = session
        .project()
        .parent()
        .must_be("lint workspace root")
        .to_path_buf();
    let rules = session.project().join("bend-lint-rules");
    fs::create_dir(&rules).must_be("create the project rule directory");
    fs::write(rules.join("rules.bend"), RULE_PROGRAM).must_be("write the project rule program");
    let shared = workspace.join("shared");
    fs::create_dir(&shared).must_be("create the external rule directory");
    let helper = shared.join("helper.bend");
    fs::write(&helper, RULE_HELPER).must_be("write the external rule helper");
    fs::write(session.control.join("prepare-block"), "").must_be("gate the first preparation");
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.wait_file("prepare-address");
    assert_eq!(
        session.calls(),
        0,
        "the first check is still preparing before it can run the analyzer"
    );

    fs::write(&helper, CHANGED_RULE_HELPER).must_be("change the external rule helper");
    session.client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&helper),"type":2}]}),
    );
    fs::write(session.control.join("prepare-release"), "").must_be("release the preparation gate");

    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint_code(message, "language/unneeded_plus")
        })
        .must_be("the external rule helper change publishes a completed check");
    assert_eq!(packet["params"]["version"], 1);
    assert_eq!(
        session.calls(),
        1,
        "the fenced first check never reached the analyzer"
    );
    assert_eq!(
        session.prepares(),
        2,
        "the recaptured helper bytes are a new preparation input"
    );
    let deadline = Instant::now() + TIMEOUT;
    while session.status()["state"] != "ready" {
        assert!(
            Instant::now() < deadline,
            "the rechecked entry completes instead of staying skipped: {}",
            session.status()
        );
        thread::sleep(Duration::from_millis(10));
    }
    session.finish();
}

#[test]
fn enabling_lint_checks_an_already_open_document() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.open(SOURCE);
    let symbols = session.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":session.document}}),
    );
    assert_eq!(symbols["result"][0]["name"], "main");

    session.configure(true, false, 10_000);
    let packet = session.lint();
    let diagnostics = packet["params"]["diagnostics"]
        .as_array()
        .must_be("published diagnostics");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["source"] == "bend-lint"
            && diagnostic["code"] == "language/unneeded_plus"
            && diagnostic["range"] == range(0, 9, 10)
    }));
    session.finish();
}

// Regression: the entry was opened while no ancestor configuration existed, so
// it is owned by its document, not by a project. Creating the configuration is
// an input change of that same open entry: the watcher notification must check
// it under its new project ownership instead of leaving the entry skipped.
#[test]
fn creating_a_project_configuration_checks_an_already_open_entry() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    let config = session.project().join("bend-lint.json");
    fs::remove_file(&config).must_be("start without a project configuration");
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let deadline = Instant::now() + TIMEOUT;
    while session.status()["state"] != "skipped" {
        assert!(
            Instant::now() < deadline,
            "the opened entry reports the missing project configuration: {}",
            session.status()
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        session.calls(),
        0,
        "no analyzer runs while the project has no configuration"
    );

    fs::write(&config, "{\"rules\":{}}").must_be("create the project configuration");
    session.client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&config),"type":1}]}),
    );

    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint_code(message, "language/unneeded_plus")
        })
        .must_be("creating the project configuration checks the already-open entry");
    assert!(
        packet["params"]["diagnostics"]
            .as_array()
            .must_be("checked entry diagnostics")
            .iter()
            .any(|diagnostic| {
                diagnostic["source"] == "bend-lint"
                    && diagnostic["code"] == "language/unneeded_plus"
            })
    );
    let deadline = Instant::now() + TIMEOUT;
    while session.status()["state"] != "ready" {
        assert!(
            Instant::now() < deadline,
            "the migrated entry reached a completed check: {}",
            session.status()
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        session.calls(),
        1,
        "the migrated entry was checked exactly once"
    );
    session.finish();
}

#[test]
fn watched_config_change_rechecks_an_open_document() {
    let mut session = Session::new(SOURCE, &[]);
    session.standard_response();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.lint();

    session.response(
        vec![finding(&session.root, "fresh/config", range(0, 9, 10))],
        vec![],
        true,
    );
    let config = session.project().join("bend-lint.json");
    fs::write(&config, "{\"rules\":{},\"entries\":[\"main.bend\"]}")
        .must_be("change the saved project configuration");
    session.client.notify(
        "workspace/didChangeWatchedFiles",
        json!({"changes":[{"uri":uri(&config),"type":2}]}),
    );

    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["params"]["uri"] == session.document && has_lint_code(message, "fresh/config")
        })
        .must_be("the watched configuration publishes a freshly completed lint result");
    assert_eq!(packet["params"]["version"], 1);
    let diagnostics = packet["params"]["diagnostics"]
        .as_array()
        .must_be("fresh configuration diagnostics");
    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic["source"] == "bend-lint" && diagnostic["code"] == "fresh/config"
    }));
    assert!(
        !diagnostics
            .iter()
            .any(|diagnostic| diagnostic["code"] == "language/unneeded_plus")
    );
    session.finish();
}
