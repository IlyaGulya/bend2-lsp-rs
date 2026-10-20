use super::*;

#[test]
#[ignore = "requires explicit BEND2_LINT_TEST_PATH and genuine installed bend-lint"]
fn explicit_checks_preserve_another_documents_genuine_fix() {
    let (executable, arguments) = configured_analyzer();
    let directory = tempdir().must_be("genuine lint command workspace");
    let project = directory.path().join("genuine analyzer project 🦀");
    fs::create_dir(&project).must_be("create analyzer project");
    let source = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n    Nat.add(x, y)\n";
    let canonical = "import Base\n\ndef f(+x: Nat, y: Nat) -> Nat:\n  Nat.add(x, y)\n";
    let first = project.join("first.bend");
    let second = project.join("second.bend");
    for path in [&first, &second] {
        fs::write(path, source).must_be("write independent saved source");
    }
    fs::write(
        project.join("bend-lint.json"),
        "{\"packages\":{\"bend-lint-rules\":\"0.1.1.0\"},\"rules\":{\"bend-lint-rules/language/unneeded_plus\":{},\"bend-lint-rules/layout/format\":{}}}",
    ).must_be("enable genuine rules");
    // Preparing trusted rules is an installation step, not part of the LSP's
    // bounded check. A cold native build can exceed the provider's deadline.
    genuine_report(&executable, &arguments, &first);
    let started = Instant::now();
    let mut client = configured_client(&project, &executable, &arguments);
    let documents = [uri(&first), uri(&second)];
    for document in &documents {
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":document,"languageId":"bend","version":1,"text":source
            }}),
        );
    }
    for document in &documents {
        client
            .receive_matching(Duration::from_secs(10), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == *document
                    && message["params"]["version"] == 1
            })
            .must_be("opened source is ready before explicit checking");
    }
    let mut first_format = None;
    for document in &documents {
        client.request(
            "workspace/executeCommand",
            json!({
                "command":"bend2.lint.run","arguments":[{"uri":document}]
            }),
        );
        let packet = wait_for_saved_diagnostics(&mut client, document, started, 1);
        if document == &documents[0] {
            first_format = Some(
                finding(
                    &packet["params"]["diagnostics"],
                    "bend-lint-rules/layout/format",
                )
                .clone(),
            );
        }
    }
    let diagnostic = first_format.must_be("first source has a genuine formatter finding");
    let response = client.request("textDocument/codeAction", json!({
        "textDocument":{"uri":documents[0]},"range":diagnostic["range"],
        "context":{"diagnostics":[diagnostic],"triggerKind":1,"only":["quickfix.bend-lint.dangerous"]}
    }));
    let actions = response["result"].as_array().must_be("explicit fixes");
    assert_eq!(
        actions.len(),
        1,
        "checking the second entry preserves the first entry's fix"
    );
    assert_eq!(
        actions[0]["edit"]["documentChanges"][0]["textDocument"],
        json!({"uri":documents[0],"version":1})
    );
    assert_eq!(
        actions[0]["edit"]["documentChanges"][0]["edits"][0]["newText"],
        canonical
    );
    for document in &documents {
        let status = client.request(
            "workspace/executeCommand",
            json!({
                "command":"bend2.lint.status","arguments":[{"uri":document}]
            }),
        );
        assert_eq!(status["result"]["state"], "ready");
        assert_eq!(status["result"]["document"], *document);
    }
    // Settings updates may reload the compiler's indexed prelude; that index
    // commit must not invalidate unchanged on-disk analyzer inputs.
    configure_analyzer(&mut client, &executable, &arguments);
    save_formatted_source(&mut client, &first, &documents[0], canonical, started);
    client.finish();
}

fn save_formatted_source(
    client: &mut LspClient,
    path: &Path,
    document: &str,
    canonical: &str,
    started: Instant,
) {
    fs::write(path, canonical).must_be("apply the explicit user edit to disk");
    client.notify(
        "textDocument/didChange",
        json!({
            "textDocument":{"uri":document,"version":2},
            "contentChanges":[{"text":canonical}]
        }),
    );
    // Real editors send these consecutively, without a request acknowledging
    // the intervening snapshot build.
    client.notify(
        "textDocument/didSave",
        json!({"textDocument":{"uri":document}}),
    );
    let fixed = wait_for_saved_diagnostics(client, document, started, 2);
    let remaining = fixed["params"]["diagnostics"]
        .as_array()
        .must_be("remaining findings");
    assert!(
        remaining
            .iter()
            .any(|item| item["code"] == "bend-lint-rules/language/unneeded_plus")
    );
    assert!(
        remaining
            .iter()
            .all(|item| item["code"] != "bend-lint-rules/layout/format")
    );
}
