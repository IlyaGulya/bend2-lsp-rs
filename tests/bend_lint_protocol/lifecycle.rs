use super::*;

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
