use super::*;

fn disabled_session(dependencies: &[(&str, &str)]) -> Session {
    let directory = tempdir().must_be("disabled action workspace");
    let project = directory.path().join("project with spaces 🦀");
    let control = directory.path().join("control");
    fs::create_dir(&project).must_be("create project");
    fs::create_dir(&control).must_be("create fixture controls");
    fs::write(project.join("bend-lint.json"), "{\"rules\":{}}")
        .must_be("write analyzer project config");
    let root = project.join("main.bend");
    fs::write(&root, SOURCE).must_be("write saved source");
    for (name, source) in dependencies {
        fs::write(project.join(name), source).must_be("write saved dependency");
    }
    fs::write(
        control.join("response.json"),
        "{\"ok\":true,\"findings\":[],\"suppressed\":[],\"skipped\":[]}",
    )
    .must_be("initialize analyzer response");
    fs::write(control.join("exit-code"), "0").must_be("initialize analyzer exit");
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
    let root_uri = Url::from_directory_path(&project).must_be("workspace URI");
    let response = client.request(
        "initialize",
        json!({"processId":null,"rootUri":root_uri,
            "workspaceFolders":[{"uri":root_uri,"name":"test"}],
            "capabilities":{"textDocument":{"codeAction":{"disabledSupport":true}}}}),
    );
    assert!(response["result"]["capabilities"].is_object());
    client.notify("initialized", json!({}));
    Session {
        document: uri(&root),
        directory,
        control,
        root,
        client,
    }
}

#[test]
fn disabled_dangerous_fix_explains_missing_optin_without_edit_or_command() {
    let mut session = disabled_session(&[]);
    let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
    item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![item], vec![], true);
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    assert!(session.actions(diagnostics.clone(), 2).is_empty());
    let actions = session.actions(diagnostics, 1);
    assert_eq!(
        actions.len(),
        1,
        "the real suggested fix explains its refusal"
    );
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    let reason = action["disabled"]["reason"]
        .as_str()
        .must_be("disabled reason");
    assert!(
        reason.contains("bend.lint.dangerousCodeActions"),
        "{reason}"
    );
    assert!(action["edit"].is_null());
    assert!(action["command"].is_null());
    assert_ne!(action["isPreferred"], true);
    session.finish();
}

#[test]
fn disabled_capability_preserves_explicit_versioned_fix_and_automatic_safety() {
    let mut session = disabled_session(&[]);
    let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
    item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![item], vec![], true);
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    assert!(session.actions(diagnostics.clone(), 2).is_empty());
    let actions = session.actions(diagnostics, 1);
    assert_eq!(actions.len(), 1);
    assert!(actions[0]["disabled"].is_null());
    assert!(actions[0]["command"].is_null());
    assert_ne!(actions[0]["isPreferred"], true);
    assert_eq!(
        actions[0]["edit"]["documentChanges"],
        json!([{"textDocument":{"uri":session.document,"version":1},
            "edits":[{"range":range(0, 9, 10),"newText":""}]}])
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("saved source"),
        SOURCE
    );
    session.finish();
}

#[test]
fn closed_multifile_target_has_disabled_reason_instead_of_partial_edit() {
    let dependency_source = "def value: U32\n  1\n";
    let mut session = disabled_session(&[("dep.bend", dependency_source)]);
    let dependency = session.project().join("dep.bend");
    let mut item = finding(&session.root, "custom/multifile", range(0, 9, 10));
    item["fixes"] = json!([{"title":"Change both files","applicability":"dangerous","edits":[
        {"path":session.root,"range":range(0, 9, 10),"text":""},
        {"path":dependency,"range":range(1, 2, 3),"text":"2"}
    ]}]);
    session.response(vec![item], vec![], true);
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    let actions = session.actions(diagnostics, 1);
    assert_eq!(actions.len(), 1);
    let reason = actions[0]["disabled"]["reason"]
        .as_str()
        .must_be("closed target reason");
    assert!(reason.to_ascii_lowercase().contains("open"), "{reason}");
    assert!(
        actions[0]["edit"].is_null(),
        "never offer a partial multi-file edit"
    );
    assert!(actions[0]["command"].is_null());
    assert_eq!(
        fs::read_to_string(&dependency).must_be("saved dependency"),
        dependency_source
    );
    session.finish();
}

#[test]
fn legacy_client_can_explain_refusal_but_dangerous_only_request_stays_empty() {
    let mut session = Session::new(SOURCE, &[]);
    let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
    item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![item], vec![], true);
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    assert!(session.actions(diagnostics.clone(), 1).is_empty());
    let response = session.client.request(
        "textDocument/codeAction",
        json!({"textDocument":{"uri":session.document},"range":range(0, 0, 24),
            "context":{"triggerKind":1,"only":["quickfix"],"diagnostics":diagnostics}}),
    );
    let actions = response["result"].as_array().must_be("explanation actions");
    assert_eq!(actions.len(), 1);
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.explain");
    assert!(action["edit"].is_null());
    assert_eq!(action["command"]["command"], "bend2.lint.explain");
    assert_eq!(action["command"]["arguments"][0]["uri"], session.document);
    let reason = action["command"]["arguments"][0]["reason"]
        .as_str()
        .must_be("safe explanation reason");
    assert!(reason.contains("bend.lint.dangerousCodeActions"));
    session
        .client
        .request("workspace/executeCommand", action["command"].clone());
    let shown = session.client.receive_matching(TIMEOUT, |message| {
        message["method"] == "window/showMessage" && message["params"]["message"] == reason
    });
    assert!(
        shown.is_some(),
        "the explanation command displays only the refusal"
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("untouched source"),
        SOURCE
    );
    session.finish();
}

#[test]
fn malformed_suggested_edits_are_explained_without_exposing_an_edit() {
    let mut session = disabled_session(&[]);
    let mut item = finding(&session.root, "custom/invalid-fixes", range(0, 9, 10));
    let mut unsupported = fix(&session.root, range(0, 9, 10), "");
    unsupported["applicability"] = json!("safe");
    item["fixes"] = json!([
        fix(&session.root, range(0, 99, 100), ""),
        {"title":"Overlapping","applicability":"dangerous","edits":[
            {"path":session.root,"range":range(0, 9, 11),"text":""},
            {"path":session.root,"range":range(0, 10, 12),"text":""}
        ]},
        unsupported
    ]);
    session.response(vec![item], vec![], true);
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    let actions = session.actions(diagnostics, 1);
    assert_eq!(actions.len(), 3);
    for action in &actions {
        assert!(action["disabled"]["reason"].is_string());
        assert!(action["edit"].is_null());
        assert!(action["command"].is_null());
    }
    session.finish();
}

// LSP makes CodeActionContext.triggerKind optional. A client that sends only
// `only` and `diagnostics` is still invoking actions, so it must receive the
// same exact versioned dangerous fix as an explicit INVOKED request.
#[test]
fn omitted_trigger_kind_returns_the_exact_versioned_dangerous_fix() {
    let mut session = disabled_session(&[]);
    let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
    item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![item], vec![], true);
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    let params = json!({
        "textDocument":{"uri":session.document},
        "range":range(0, 9, 10),
        "context":{"only":["quickfix.bend-lint.dangerous"],"diagnostics":diagnostics}
    });
    let response = session.client.request("textDocument/codeAction", params);
    let actions = response["result"]
        .as_array()
        .must_be("omitted triggerKind actions");
    assert_eq!(
        actions.len(),
        1,
        "an omitted triggerKind still invokes the requested dangerous fix"
    );
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    assert!(action["disabled"].is_null());
    assert!(action["command"].is_null());
    assert_ne!(action["isPreferred"], true);
    assert_eq!(
        action["edit"]["documentChanges"],
        json!([{"textDocument":{"uri":session.document,"version":1},
            "edits":[{"range":range(0, 9, 10),"newText":""}]}]),
        "the omitted triggerKind response carries the current version and exact range"
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("saved source"),
        SOURCE,
        "the action only proposes an edit"
    );
    // Explicit AUTOMATIC remains rejected even with the same narrow context.
    let automatic = session.client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":session.document},
            "range":range(0, 9, 10),
            "context":{"triggerKind":2,"only":["quickfix.bend-lint.dangerous"],
                "diagnostics":diagnostics}
        }),
    );
    assert!(
        automatic["result"]
            .as_array()
            .must_be("automatic actions")
            .is_empty(),
        "an automatic trigger never exposes a dangerous fix"
    );
    session.finish();

    // Without the opt-in the same omitted-triggerKind request may only explain
    // the refusal; it must never produce a runnable dangerous edit.
    let mut not_opted_in = disabled_session(&[]);
    let mut item = finding(
        &not_opted_in.root,
        "language/unneeded_plus",
        range(0, 9, 10),
    );
    item["fixes"] = json!([fix(&not_opted_in.root, range(0, 9, 10), "")]);
    not_opted_in.response(vec![item], vec![], true);
    not_opted_in.configure(true, false, 10_000);
    not_opted_in.open(SOURCE);
    not_opted_in.save();
    let diagnostics = not_opted_in.lint()["params"]["diagnostics"].clone();
    let response = not_opted_in.client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":not_opted_in.document},
            "range":range(0, 9, 10),
            "context":{"only":["quickfix.bend-lint.dangerous"],"diagnostics":diagnostics}
        }),
    );
    let actions = response["result"]
        .as_array()
        .must_be("omitted triggerKind refusal");
    assert_eq!(actions.len(), 1, "the refusal is explained once");
    assert!(actions[0]["edit"].is_null());
    assert!(actions[0]["command"].is_null());
    let reason = actions[0]["disabled"]["reason"]
        .as_str()
        .must_be("opt-in refusal reason");
    assert!(
        reason.contains("bend.lint.dangerousCodeActions"),
        "{reason}"
    );
    assert_eq!(
        fs::read_to_string(&not_opted_in.root).must_be("untouched source"),
        SOURCE
    );
    not_opted_in.finish();
}

#[test]
fn stale_client_diagnostic_requests_recheck_without_inventing_a_fix() {
    let mut session = disabled_session(&[]);
    session.standard_response();
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    session.change(2, "# unsaved\n");
    let actions = session.actions(diagnostics, 1);
    assert_eq!(actions.len(), 1);
    assert!(actions[0]["disabled"]["reason"].is_string());
    assert!(actions[0]["edit"].is_null());
    assert!(actions[0]["command"].is_null());
    session.finish();
}
