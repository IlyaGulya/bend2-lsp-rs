use super::*;

#[test]
fn explicit_run_checks_saved_document_without_save_and_reports_selected_identity() {
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
        "explicit saved-document run must be supported: {run}"
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
fn dirty_document_run_is_skipped_with_save_guidance_and_does_not_execute() {
    let mut session = Session::new(SOURCE, &[]);
    session.configure(true, false, 2_000);
    session.open(SOURCE);
    session.change(2, "def main(+x: U32) -> U32:\n  0\n");
    command(&mut session, "bend2.lint.run");
    session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "window/showMessage"
                && message["params"]["message"].as_str().is_some_and(|text| {
                    text.contains("bend-lint") && text.to_ascii_lowercase().contains("save")
                })
        })
        .must_be("dirty saved-input guidance");
    let status = command(&mut session, "bend2.lint.status");
    assert_eq!(status["state"], "skipped", "{status}");
    assert!(!session.control.join("calls").exists());
    session.no_lint();
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
