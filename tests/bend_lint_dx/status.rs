use super::*;

#[test]
fn explicit_run_checks_open_document_and_reports_prepared_identity() {
    let mut session = Session::new(SOURCE, &[]);
    session.configure(true, false, 2_000);
    session.standard_response();
    session.open(SOURCE);

    let run = session.client.request_response(
        "workspace/executeCommand",
        json!({"command":"bend2.lint.run","arguments":[{"uri":session.document}]}),
    );
    assert!(
        run.get("error").is_none(),
        "explicit open-document run must be supported: {run}"
    );
    let diagnostics = session.lint();
    assert_eq!(
        diagnostics["params"]["diagnostics"][0]["code"],
        "language/unneeded_plus"
    );
    let status = session.client.request(
        "workspace/executeCommand",
        json!({"command":"bend2.lint.status","arguments":[{"uri":session.document}]}),
    );
    assert_eq!(status["result"]["state"], "ready");
    assert_eq!(status["result"]["document"], session.document);
    assert_eq!(
        status["result"]["executable"],
        executable().to_string_lossy().as_ref()
    );
    assert!(
        status["result"]["serverExecutable"]
            .as_str()
            .is_some_and(|path| !path.is_empty())
    );
    assert_eq!(status["result"]["partial"], false);
    assert_eq!(status["result"]["skippedChecks"], 0);
    assert!(
        status["result"]["toolIdentity"]
            .as_str()
            .must_be("prepared identity")
            .contains("bend-lint 0.2.1 prepared"),
        "status reports the real preparation identity: {status}"
    );
    assert!(
        status["result"]["toolIdentity"]
            .as_str()
            .is_some_and(|identity| identity.contains("backend native")
                && identity.contains("runtime bun 1.4.4")),
        "preparation identity carries the fixture's actual report: {status}"
    );
    session.finish();
}

fn command(session: &mut Session, name: &str) -> Value {
    session.client.request(
        "workspace/executeCommand",
        json!({"command":name,"arguments":[{"uri":session.document}]}),
    )["result"]
        .clone()
}

#[test]
fn explicit_commands_remain_disabled_without_external_execution_consent() {
    let mut session = Session::new(SOURCE, &[]);
    session.open(SOURCE);
    let status = command(&mut session, "bend2.lint.run");
    assert_eq!(status["state"], "disabled");
    assert!(
        status["reason"]
            .as_str()
            .must_be("consent reason")
            .contains("bend.lint.enabled")
    );
    session.no_lint();
    assert!(
        !session.control.join("calls").exists(),
        "run must not bypass consent"
    );
    session.finish();
}

#[test]
fn configuration_typos_and_misplaced_settings_are_explained_without_execution() {
    for (settings, expected) in [
        (
            json!({"bend":{"lint":{"enabled":true,"path":executable(),"enable":true}}}),
            "enable",
        ),
        (
            json!({"bend2-lsp":{"lint":{"enabled":true,"path":executable()}}}),
            "bend2-lsp.lint",
        ),
    ] {
        let mut session = Session::new(SOURCE, &[]);
        session.client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":settings}),
        );
        session.open(SOURCE);
        let status = command(&mut session, "bend2.lint.run");
        assert_eq!(status["state"], "disabled");
        assert!(
            status["reason"]
                .as_str()
                .must_be("configuration explanation")
                .contains(expected),
            "{status}"
        );
        session.no_lint();
        assert!(!session.control.join("calls").exists());
        session.finish();
    }
}

#[test]
fn explicit_source_actions_offer_run_and_status_but_not_during_automatic_requests() {
    let mut session = Session::new(SOURCE, &[]);
    session.open(SOURCE);
    for trigger in [1, 2] {
        let actions = session.client.request(
            "textDocument/codeAction",
            json!({
                "textDocument":{"uri":session.document},"range":range(0,0,0),
                "context":{"diagnostics":[],"triggerKind":trigger,"only":["source.bend2Lint"]}
            }),
        );
        let actions = actions["result"].as_array().must_be("source actions");
        if trigger == 1 {
            assert!(
                actions
                    .iter()
                    .any(|action| action["kind"] == "source.bend2Lint.run"
                        && action["command"]["command"] == "bend2.lint.run")
            );
            assert!(
                actions
                    .iter()
                    .any(|action| action["kind"] == "source.bend2Lint.status"
                        && action["command"]["command"] == "bend2.lint.status")
            );
            assert!(actions.iter().all(|action| action.get("edit").is_none()));
        } else {
            assert!(
                actions.is_empty(),
                "automatic requests must not advertise operational actions"
            );
        }
    }
    session.finish();
}

#[test]
fn partial_checks_are_distinct_from_complete_and_keep_valid_findings() {
    let mut session = Session::new(SOURCE, &[("dep.bend", SOURCE)]);
    session.response_with_skips(
        vec![finding(
            &session.root,
            "language/unneeded_plus",
            range(0, 9, 10),
        )],
        vec![],
        vec![json!({
            "path":"dep.bend",
            "reason":"unchecked",
            "rules":["performance/append_in_loop"],
            "message":"bend-lint: Bend's check of dep.bend fails, so the rules that need its facts did not run on it: performance/append_in_loop"
        })],
        true,
    );
    session.configure(true, false, 2_000);
    session.open(SOURCE);
    let packet = session.lint();
    let diagnostics = packet["params"]["diagnostics"]
        .as_array()
        .must_be("lint diagnostics");
    assert!(
        diagnostics
            .iter()
            .any(|item| item["code"] == "language/unneeded_plus"),
        "findings from checked inputs are retained: {packet}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|item| item["code"] == "language/unneeded_plus" || item["source"] != "bend-lint"),
        "a skipped check never becomes a fabricated diagnostic: {packet}"
    );
    session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "window/logMessage"
                && message["params"]["message"].as_str().is_some_and(|text| {
                    text.contains("dep.bend")
                        && text.contains("unchecked")
                        && text.contains("performance/append_in_loop")
                })
        })
        .must_be("skipped reasons are exposed operationally");
    let status = command(&mut session, "bend2.lint.status");
    assert_eq!(status["state"], "ready", "{status}");
    assert_eq!(status["partial"], true, "{status}");
    assert_eq!(status["skippedChecks"], 1, "{status}");
    assert_eq!(
        fs::read_to_string(session.project().join("dep.bend")).must_be("unchanged dependency"),
        SOURCE
    );
    session.finish();
}

#[test]
fn missing_project_config_is_visible_as_skipped_without_source_diagnostics() {
    let mut session = Session::new(SOURCE, &[]);
    fs::remove_file(session.project().join("bend-lint.json")).must_be("remove project config");
    session.configure(true, false, 2_000);
    session.open(SOURCE);
    command(&mut session, "bend2.lint.run");
    let message = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "window/showMessage"
                && message["params"]["message"]
                    .as_str()
                    .is_some_and(|text| text.contains("bend-lint.json"))
        })
        .must_be("missing config explanation");
    assert!(
        message["params"]["message"]
            .as_str()
            .must_be("config explanation")
            .contains("bend-lint")
    );
    let status = command(&mut session, "bend2.lint.status");
    assert_eq!(status["state"], "skipped");
    assert!(status["projectConfig"].is_null());
    assert!(!session.control.join("calls").exists());
    session.no_lint();
    session.finish();
}

#[test]
fn running_failure_and_recovery_are_observable_without_operational_source_errors() {
    let mut session = Session::new(SOURCE, &[]);
    session.configure(true, false, 2_000);
    session.open(SOURCE);
    session.block();
    fs::write(
        session.control.join("response.json"),
        "invalid analyzer response",
    )
    .must_be("invalid protocol output");
    command(&mut session, "bend2.lint.run");
    session.wait_file("started");
    assert_eq!(
        command(&mut session, "bend2.lint.status")["state"],
        "running"
    );
    session.release();
    session.failure();
    let failed = command(&mut session, "bend2.lint.status");
    assert_eq!(failed["state"], "failed", "{failed}");
    assert!(
        !failed["reason"]
            .as_str()
            .must_be("failure explanation")
            .is_empty()
    );
    session.no_lint();
    session.standard_response();
    command(&mut session, "bend2.lint.run");
    session.lint();
    assert_eq!(command(&mut session, "bend2.lint.status")["state"], "ready");
    session.finish();
}

#[test]
fn dirty_document_run_analyzes_unsaved_inputs_without_save_guidance() {
    let mut session = Session::new(SOURCE, &[]);
    session.configure(true, false, 2_000);
    session.standard_response();
    session.open(SOURCE);
    let edited = "def main(+x: U32) -> U32:\n  0\n";
    session.change(2, edited);
    let run = session.client.request_response(
        "workspace/executeCommand",
        json!({"command":"bend2.lint.run","arguments":[{"uri":session.document}]}),
    );
    assert!(
        run.get("error").is_none(),
        "an explicit run must accept a dirty editor buffer: {run}"
    );
    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && message["params"]["version"] == 2
                && has_lint(message)
        })
        .must_be("the dirty buffer is analyzed without saving");
    assert_eq!(packet["params"]["version"], 2);
    let root = fs::canonicalize(&session.root).must_be("canonical source");
    assert_eq!(
        session.overlay_input()[root.to_string_lossy().as_ref()],
        edited,
        "the run analyzes the unsaved text"
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("unchanged disk"),
        SOURCE
    );
    let status = command(&mut session, "bend2.lint.status");
    assert_eq!(status["state"], "ready", "{status}");
    assert!(
        !status["reason"]
            .as_str()
            .must_be("ready reason")
            .to_ascii_lowercase()
            .contains("save"),
        "dirty .bend inputs are analyzed, never answered with save guidance: {status}"
    );
    session.finish();
}

#[test]
fn missing_selected_tool_is_actionable_and_explain_never_runs_analysis() {
    let mut session = Session::new(SOURCE, &[]);
    let missing = session.project().join("missing-analyzer");
    session.client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{
            "bend2-lsp":{"compilerPath":executable()},
            "bend":{"lint":{"enabled":true,"path":missing}}
        }}),
    );
    session.open(SOURCE);
    let status = command(&mut session, "bend2.lint.status");
    assert_eq!(status["state"], "disabled");
    assert!(
        status["reason"]
            .as_str()
            .must_be("missing tool reason")
            .contains("missing-analyzer")
    );
    let reason = "This fix is disabled because the project input changed; save and run again.";
    session.client.request(
        "workspace/executeCommand",
        json!({
            "command":"bend2.lint.explain","arguments":[{"uri":session.document,"reason":reason}]
        }),
    );
    session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "window/showMessage" && message["params"]["message"] == reason
        })
        .must_be("explanation displays supplied reason");
    assert!(!session.control.join("calls").exists());
    session.finish();
}

#[test]
fn repeated_identical_failure_updates_status_without_repeating_problem_popup() {
    let mut session = Session::new(SOURCE, &[]);
    session.configure(true, false, 2_000);
    session.open(SOURCE);
    fs::write(
        session.control.join("response.json"),
        "invalid analyzer response",
    )
    .must_be("invalid protocol output");
    command(&mut session, "bend2.lint.run");
    let first_problem = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "window/showMessage"
                && message["params"]["message"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("bend-lint failed for "))
        })
        .must_be("first analyzer failure popup");
    assert_eq!(
        command(&mut session, "bend2.lint.status")["state"],
        "failed"
    );
    fs::remove_file(session.control.join("started")).must_be("reset fixture start acknowledgment");
    session.block();
    command(&mut session, "bend2.lint.run");
    session.wait_file("started");
    assert_eq!(
        command(&mut session, "bend2.lint.status")["state"],
        "running"
    );
    session.release();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let status = command(&mut session, "bend2.lint.status");
        if status["state"] == "failed" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "second failure did not become terminal: {status}"
        );
    }
    assert!(
        session
            .client
            .receive_matching(QUIET, |message| {
                message["method"] == "window/showMessage"
                    && message["params"]["message"] == first_problem["params"]["message"]
            })
            .is_none(),
        "identical failures must not repeatedly interrupt the developer"
    );
    session.no_lint();
    session.finish();
}
