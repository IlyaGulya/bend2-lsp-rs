use super::*;

fn document_symbols(session: &mut Session, document: &str) -> Vec<String> {
    session.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":document}}),
    )["result"]
        .as_array()
        .map(|symbols| {
            symbols
                .iter()
                .filter_map(|symbol| symbol["name"].as_str())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

// Two editor spellings of one file are independent open buffers. Percent-encoded
// unreserved letters keep distinct URI strings while decoding to the same
// normalized lexical path, and neither spelling may replace the other's text,
// version stream, or close lifecycle.
#[test]
fn equivalent_uri_spellings_keep_independent_open_buffers_and_versions() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR)
        .must_be("compiler error fixture");

    // Verify the fixture premise: the spellings stay distinct URIs but decode
    // to one path. (`./` and `..` segments would collapse during URL parsing.)
    let canonical = Url::parse(&session.document).must_be("document URI");
    let alias_text = session.document.replacen("/main.bend", "/%6Dain.bend", 1);
    let alias = Url::parse(&alias_text).must_be("alias document URI");
    assert_ne!(
        alias_text, session.document,
        "the alias keeps a distinct URI string"
    );
    assert_eq!(
        alias.to_file_path().must_be("alias path"),
        canonical.to_file_path().must_be("canonical path"),
        "both spellings decode to one normalized lexical path"
    );

    let first = "def first(x: U32) -> U32:\n  x\n";
    let third = "def third(x: U32) -> U32:\n  x\n";
    let canonical_text = session.document.clone();

    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":canonical_text,"languageId":"bend","version":3,"text":first}}),
    );
    session.compiler(3);
    assert_eq!(document_symbols(&mut session, &canonical_text), ["first"]);

    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":alias_text,"languageId":"bend","version":11,"text":first}}),
    );
    // Same-content spellings coalesce for analysis but each publishes its own
    // open version.
    session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == alias_text
                && message["params"]["version"] == 11
        })
        .must_be("the alias publishes its own open version");

    assert_eq!(
        document_symbols(&mut session, &canonical_text),
        ["first"],
        "opening another spelling must not replace the canonical buffer"
    );
    assert_eq!(
        document_symbols(&mut session, &alias_text),
        ["first"],
        "the alias keeps its own open text"
    );

    // The canonical stream sits at 4 while the alias sits at 11: independent
    // editor version streams must not be ordered against each other, so this
    // edit must still commit on the canonical spelling only.
    session.client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":canonical_text,"version":4},
            "contentChanges":[{"text":third}]}),
    );
    assert_eq!(
        document_symbols(&mut session, &canonical_text),
        ["third"],
        "the canonical buffer advances on its own version"
    );
    assert_eq!(
        document_symbols(&mut session, &alias_text),
        ["first"],
        "editing one spelling must not rewrite the other buffer"
    );

    session.client.notify(
        "textDocument/didClose",
        json!({"textDocument":{"uri":alias_text}}),
    );
    assert_eq!(
        document_symbols(&mut session, &canonical_text),
        ["third"],
        "closing the alias must not close the canonical buffer"
    );
    session.finish();
}

// Two editor spellings of one physical source through a symlink are separate
// open buffers that share the analysis but each receive the same bend-lint
// findings at their own version. Windows symlink creation needs privileges, so
// this stays honestly Unix-gated instead of pretending portable coverage.
#[cfg(unix)]
#[test]
fn symlinked_spellings_publish_lint_findings_at_their_own_versions() {
    use std::os::unix::fs::symlink;

    let mut session = Session::new(SOURCE, &[]);
    let alias = session.project().join("alias.bend");
    symlink(&session.root, &alias).must_be("create symlinked spelling of the source");
    session.standard_response();
    session.configure(true, false, 10_000);

    let canonical_text = session.document.clone();
    let alias_text = uri(&alias);
    assert_ne!(canonical_text, alias_text);
    assert_eq!(
        fs::canonicalize(&alias).must_be("symlink identity"),
        fs::canonicalize(&session.root).must_be("source identity"),
        "the two URI spellings name one physical source"
    );

    for (text, version) in [(&canonical_text, 2), (&alias_text, 7)] {
        session.client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":text,"languageId":"bend","version":version,"text":SOURCE}}),
        );
    }

    for (text, version) in [(&canonical_text, 2_i64), (&alias_text, 7_i64)] {
        let packet = session
            .client
            .receive_matching(TIMEOUT, |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"].as_str() == Some(text.as_str())
                    && message["params"]["version"] == version
                    && has_lint(message)
            })
            .must_be("each same-source spelling publishes bend-lint findings");
        let items = packet["params"]["diagnostics"]
            .as_array()
            .must_be("published diagnostics");
        assert!(
            items.iter().any(|item| {
                item["source"] == "bend-lint" && item["code"] == "language/unneeded_plus"
            }),
            "the shared finding reaches every spelling: {packet}"
        );
    }
    session.finish();
}

// One physical project can hold several entries, and the editor may reach that
// project through a directory symlink. The two entry spellings share one
// project key, so both checks are planned in the same frozen revision; the view
// captured through the first spelling must still serve the second entry instead
// of refusing it as frozen. Each entry keeps its own editor URI and publishes
// the finding the analyzer reported for that entry.
#[cfg(unix)]
#[test]
fn directory_symlink_entries_complete_together_in_one_project_revision() {
    use std::os::unix::fs::symlink;

    let mut session = Session::new(SOURCE, &[]);
    let project = session.project().to_path_buf();
    let second = project.join("second.bend");
    fs::write(&second, SOURCE).must_be("write the second project entry");
    let alias_root = session.directory.path().join("project alias");
    symlink(&project, &alias_root).must_be("create the directory-symlink project spelling");
    let aliased_second = alias_root.join("second.bend");
    let main_text = session.document.clone();
    let second_text = uri(&aliased_second);
    assert_ne!(main_text, second_text);
    assert_eq!(
        fs::canonicalize(&aliased_second).must_be("aliased entry identity"),
        fs::canonicalize(&second).must_be("real entry identity"),
        "both entry spellings live in one physical project"
    );

    let responses = session.control.join("responses");
    fs::create_dir(&responses).must_be("create per-entry analyzer responses");
    for (name, path, code) in [
        ("main.bend.json", &session.root, "entry/main"),
        ("second.bend.json", &second, "entry/second"),
    ] {
        let response = json!({
            "ok":true,
            "findings":[finding(path, code, range(0, 0, 3))],
            "suppressed":[], "skipped":[]
        });
        fs::write(
            responses.join(name),
            serde_json::to_vec(&response).must_be("serialize per-entry response"),
        )
        .must_be("write per-entry analyzer response");
    }
    session.configure(true, false, 10_000);

    // Both notifications reach the server before the first debounced plan runs,
    // so both entries belong to the one project revision that captured a view
    // through the first spelling.
    for (text, version) in [(&main_text, 1_i64), (&second_text, 2_i64)] {
        session.client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":text,"languageId":"bend","version":version,"text":SOURCE}}),
        );
    }
    for (text, version, code) in [
        (&main_text, 1_i64, "entry/main"),
        (&second_text, 2_i64, "entry/second"),
    ] {
        let text = text.clone();
        let code = code.to_owned();
        let packet = session
            .client
            .receive_matching(TIMEOUT, |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"].as_str() == Some(text.as_str())
                    && message["params"]["version"] == version
                    && has_lint_code(message, &code)
            })
            .must_be("each entry of one physical project completes its own check");
        assert_eq!(
            packet["params"]["uri"].as_str(),
            Some(text.as_str()),
            "the finding stays on the entry spelling that requested the check"
        );
        assert!(
            packet["params"]["diagnostics"]
                .as_array()
                .must_be("published diagnostics")
                .iter()
                .any(|item| item["source"] == "bend-lint" && item["code"] == code),
            "the {code} finding is published on its entry: {packet}"
        );
        let status = session.client.request(
            "workspace/executeCommand",
            json!({"command":"bend2.lint.status","arguments":[{"uri":text}]}),
        )["result"]
            .clone();
        assert_eq!(
            status["state"], "ready",
            "{code} completed in the shared project revision instead of a frozen-view skip: {status}"
        );
    }
    session.finish();
}

// Closing the representative (also first-open) spelling must leave the other
// open spelling as the single, unambiguous editor and index view. Its own
// declaration and overlay text survive, and workspace symbols point at the
// remaining alias URI instead of the closed spelling or its different disk text.
#[test]
fn closing_the_representative_spelling_keeps_the_remaining_alias_indexed() {
    let mut session = Session::new(SOURCE, &[]);

    let canonical = Url::parse(&session.document).must_be("document URI");
    let alias_text = session.document.replacen("/main.bend", "/%6Dain.bend", 1);
    let alias = Url::parse(&alias_text).must_be("alias document URI");
    assert_ne!(alias_text, session.document);
    assert_eq!(
        alias.to_file_path().must_be("alias path"),
        canonical.to_file_path().must_be("canonical path"),
        "both spellings decode to one normalized lexical path"
    );

    let canonical_text = session.document.clone();
    let canonical_source = "def canonical_view(x: U32) -> U32:\n  x\n";
    let alias_source = "def alias_view(x: U32) -> U32:\n  x\n";

    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":canonical_text,"languageId":"bend","version":3,"text":canonical_source}}),
    );
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":alias_text,"languageId":"bend","version":9,"text":alias_source}}),
    );
    assert_eq!(
        document_symbols(&mut session, &canonical_text),
        ["canonical_view"]
    );
    assert_eq!(document_symbols(&mut session, &alias_text), ["alias_view"]);

    // The canonical stream advances to 4 while the alias sits at 9, so the
    // canonical spelling becomes the representative view that will be closed.
    session.client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":canonical_text,"version":4},
            "contentChanges":[{"text":canonical_source}]}),
    );
    assert_eq!(
        document_symbols(&mut session, &canonical_text),
        ["canonical_view"]
    );

    session.client.notify(
        "textDocument/didClose",
        json!({"textDocument":{"uri":canonical_text}}),
    );
    assert_eq!(
        document_symbols(&mut session, &alias_text),
        ["alias_view"],
        "the remaining spelling keeps its own open declaration"
    );

    let symbols = session
        .client
        .request("workspace/symbol", json!({"query":""}));
    let symbols = symbols["result"].as_array().must_be("workspace symbols");
    let alias_symbol = symbols
        .iter()
        .find(|symbol| symbol["name"] == "alias_view")
        .must_be("the remaining alias declaration stays indexed");
    assert_eq!(
        alias_symbol["location"]["uri"], alias_text,
        "the symbol resolves to the remaining alias spelling"
    );
    assert!(
        symbols
            .iter()
            .all(|symbol| symbol["location"]["uri"] != canonical_text),
        "no symbol points at the closed representative spelling: {symbols:?}"
    );
    assert!(
        symbols.iter().all(|symbol| symbol["name"] != "main"),
        "the closed spelling's differing disk text is not indexed: {symbols:?}"
    );
    session.finish();
}

// Two same-text spellings of one file are both current editor targets. An
// explicit dangerous-fix request on one of them must not silently pick an
// arbitrary alias/version: the refusal is surfaced as a disabled action with no
// edit and no command, not an empty result and not a partial edit.
#[test]
fn dangerous_fix_refuses_an_ambiguous_equivalent_uri_target() {
    let mut session = Session::with_server_capabilities(
        SOURCE,
        &[],
        &json!({"textDocument":{"codeAction":{"disabledSupport":true}}}),
    );
    let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
    item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![item], vec![], true);
    session.configure(true, true, 10_000);

    let canonical_text = session.document.clone();
    let alias_text = session.document.replacen("/main.bend", "/%6Dain.bend", 1);
    assert_ne!(canonical_text, alias_text);
    assert_eq!(
        Url::parse(&alias_text)
            .must_be("alias URI")
            .to_file_path()
            .must_be("alias path"),
        Url::parse(&canonical_text)
            .must_be("canonical URI")
            .to_file_path()
            .must_be("canonical path"),
        "both spellings decode to one normalized lexical path"
    );

    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":canonical_text,"languageId":"bend","version":3,"text":SOURCE}}),
    );
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":alias_text,"languageId":"bend","version":9,"text":SOURCE}}),
    );

    // Use the alias's own published packet: baseline keeps only the last
    // spelling's URI, so the canonical packet is not a reliable seam there.
    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"].as_str() == Some(alias_text.as_str())
                && message["params"]["version"] == 9_i64
                && has_lint(message)
        })
        .must_be("the alias spelling receives its own versioned lint finding");
    let diagnostics = packet["params"]["diagnostics"].clone();

    let response = session.client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":alias_text},
            "range":range(0, 0, 24),
            "context":{
                "triggerKind":1,
                "only":["quickfix.bend-lint.dangerous"],
                "diagnostics":diagnostics
            }
        }),
    );
    let actions = response["result"].as_array().must_be("code actions");
    assert_eq!(
        actions.len(),
        1,
        "the offered fix is refused explicitly instead of vanishing: {actions:?}"
    );
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    assert!(
        action["disabled"]["reason"].is_string(),
        "the refusal explains why no target was chosen: {action}"
    );
    assert!(
        action["edit"].is_null(),
        "no arbitrary alias edit may be offered: {action}"
    );
    assert!(
        action["command"].is_null(),
        "no command may apply an arbitrary alias edit: {action}"
    );
    assert_ne!(action["isPreferred"], true);
    session.finish();
}

fn assert_sibling_project_checks(session: &mut Session) {
    let sibling = session.directory.path().join("sibling project");
    fs::create_dir(&sibling).must_be("create sibling project");
    fs::write(sibling.join("bend-lint.json"), "{\"rules\":{}}").must_be("write sibling config");
    let source = "def sibling_value(x: U32) -> U32:\n  x\n";
    let root = sibling.join("main.bend");
    fs::write(&root, source).must_be("write sibling source");
    let document = uri(&root);
    session.response(
        vec![finding(&root, "test/sibling_project", range(0, 4, 17))],
        vec![],
        true,
    );
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":document,"languageId":"bend","version":1,"text":source}}),
    );
    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["params"]["uri"].as_str() == Some(document.as_str())
                && has_lint_code(message, "test/sibling_project")
        })
        .must_be("the sibling project checks while the other project is conflicted");
    assert_eq!(packet["params"]["version"], 1);
}

// A conflicted project must fail closed with an actionable message naming both
// editor URIs, without preparing or running its analyzer, while an unrelated
// sibling project still checks normally. Closing the conflicting spelling then
// lets the remaining version recheck on its own, with no extra edit or save.
#[test]
fn conflicted_project_blocks_only_itself_and_close_rechecks_the_remaining_view() {
    let mut session = Session::new(SOURCE, &[]);
    session.configure(false, false, 10_000);

    let canonical_text = session.document.clone();
    let alias_text = session.document.replacen("/main.bend", "/%6Dain.bend", 1);
    assert_ne!(canonical_text, alias_text);
    assert_eq!(
        Url::parse(&alias_text)
            .must_be("alias URI")
            .to_file_path()
            .must_be("alias path"),
        Url::parse(&canonical_text)
            .must_be("canonical URI")
            .to_file_path()
            .must_be("canonical path"),
        "both spellings decode to one normalized lexical path"
    );
    let conflicting_source = "def edited_view(x: U32) -> U32:\n  x\n";

    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":canonical_text,"languageId":"bend","version":3,"text":SOURCE}}),
    );
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":alias_text,"languageId":"bend","version":9,"text":conflicting_source}}),
    );
    assert_eq!(document_symbols(&mut session, &canonical_text), ["main"]);
    assert_eq!(document_symbols(&mut session, &alias_text), ["edited_view"]);

    // Enabling lint with two different open texts is an explicit, actionable
    // refusal that names both editor spellings; it is not a silent skip.
    session.configure(true, false, 10_000);
    let warning = session
        .client
        .receive_matching(TIMEOUT, |message| {
            matches!(
                message["method"].as_str(),
                Some("window/logMessage" | "window/showMessage")
            ) && message["params"]["message"].as_str().is_some_and(|text| {
                text.contains("bend-lint")
                    && text.contains(&canonical_text)
                    && text.contains(&alias_text)
            })
        })
        .must_be("the conflict names both conflicting editor URIs");
    assert!(
        warning["params"]["message"]
            .as_str()
            .must_be("conflict message")
            .contains("bend-lint"),
        "the refusal is attributed to bend-lint: {warning}"
    );
    session.no_lint();
    assert_eq!(
        session.prepares(),
        0,
        "a conflicted project must not prepare the analyzer"
    );
    assert_eq!(
        session.calls(),
        0,
        "a conflicted project must not run the analyzer"
    );

    assert_sibling_project_checks(&mut session);

    // Restore A's own analyzer response and close only the conflicting
    // spelling: the remaining canonical version must recheck automatically.
    session.standard_response();
    let calls_before = session.calls();
    let prepares_before = session.prepares();
    session.client.notify(
        "textDocument/didClose",
        json!({"textDocument":{"uri":alias_text}}),
    );
    let refreshed = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"].as_str() == Some(canonical_text.as_str())
                && message["params"]["version"] == 3_i64
                && message["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|items| {
                        items
                            .iter()
                            .any(|item| item["code"] == "language/unneeded_plus")
                    })
        })
        .must_be("closing the conflicting spelling rechecks the remaining version");
    assert_eq!(
        refreshed["params"]["version"], 3,
        "the remaining canonical view keeps its own version"
    );
    assert_eq!(
        session.prepares(),
        prepares_before + 1,
        "the recovered project prepares its analyzer exactly once"
    );
    assert_eq!(
        session.calls(),
        calls_before + 1,
        "the recovered project runs one check for its remaining entry"
    );
    session.finish();
}

#[cfg(unix)]
fn wait_entry_ready(session: &mut Session, document: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while session.client.request(
        "workspace/executeCommand",
        json!({"command":"bend2.lint.status","arguments":[{"uri":document}]}),
    )["result"]["state"]
        != "ready"
    {
        assert!(
            Instant::now() < deadline,
            "the initial check for {document} must finish before closing it"
        );
    }
}

#[cfg(unix)]
fn explicit_dangerous_actions(
    session: &mut Session,
    document: &str,
    diagnostics: &Value,
) -> Vec<Value> {
    session.client.request(
        "textDocument/codeAction",
        json!({
            "textDocument":{"uri":document},
            "range":range(0, 0, 24),
            "context":{
                "triggerKind":1,
                "only":["quickfix.bend-lint.dangerous"],
                "diagnostics":diagnostics
            }
        }),
    )["result"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[cfg(unix)]
fn assert_disabled_refusal(actions: &[Value]) {
    assert_eq!(
        actions.len(),
        1,
        "the suggested fix is refused explicitly: {actions:?}"
    );
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    assert!(
        action["disabled"]["reason"].is_string(),
        "the refusal explains why no target was chosen: {action}"
    );
    assert!(
        action["edit"].is_null(),
        "no arbitrary alias edit may be offered: {action}"
    );
    assert!(
        action["command"].is_null(),
        "no command may apply an arbitrary alias edit: {action}"
    );
    assert_ne!(action["isPreferred"], true);
}

// A symlink spelling and its target are different lexical FileIds but one
// physical source. While both are open (same text, own versions) an explicit
// dangerous fix must not choose either; after closing one it must target the
// remaining URI and version.
#[cfg(unix)]
#[test]
fn dangerous_fix_refuses_every_uri_of_one_physically_aliased_target() {
    use std::os::unix::fs::symlink;

    let mut session = Session::with_server_capabilities(
        SOURCE,
        &[],
        &json!({"textDocument":{"codeAction":{"disabledSupport":true}}}),
    );
    let alias = session.project().join("link.bend");
    symlink(&session.root, &alias).must_be("create symlinked spelling of the source");
    let canonical_text = session.document.clone();
    let alias_text = uri(&alias);
    assert_ne!(canonical_text, alias_text);
    assert_eq!(
        fs::canonicalize(&alias).must_be("symlink identity"),
        fs::canonicalize(&session.root).must_be("source identity"),
        "the two URI spellings name one physical source"
    );

    let mut item = finding(&session.root, "language/unneeded_plus", range(0, 9, 10));
    item["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![item], vec![], true);
    session.configure(true, true, 10_000);

    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":canonical_text,"languageId":"bend","version":2,"text":SOURCE}}),
    );
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":alias_text,"languageId":"bend","version":7,"text":SOURCE}}),
    );

    let mut packets = Vec::new();
    for (document, version) in [(&canonical_text, 2_i64), (&alias_text, 7_i64)] {
        let packet = session
            .client
            .receive_matching(TIMEOUT, |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"].as_str() == Some(document.as_str())
                    && message["params"]["version"] == version
                    && has_lint(message)
            })
            .must_be("every spelling publishes its own versioned lint packet");
        packets.push(packet["params"]["diagnostics"].clone());
    }

    assert_disabled_refusal(&explicit_dangerous_actions(
        &mut session,
        &alias_text,
        &packets[1],
    ));
    assert_disabled_refusal(&explicit_dangerous_actions(
        &mut session,
        &canonical_text,
        &packets[0],
    ));
    assert_eq!(
        fs::read_to_string(&session.root).must_be("unchanged disk"),
        SOURCE
    );

    for document in [&canonical_text, &alias_text] {
        wait_entry_ready(&mut session, document);
    }

    // Close the symlink spelling; the fresh result over the remaining canonical
    // view must offer its fix for exactly that URI and version.
    let mut remaining = finding(&session.root, "test/remaining", range(0, 9, 10));
    remaining["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![remaining], vec![], true);
    session.client.notify(
        "textDocument/didClose",
        json!({"textDocument":{"uri":alias_text}}),
    );
    let refreshed = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"].as_str() == Some(canonical_text.as_str())
                && message["params"]["version"] == 2_i64
                && has_lint_code(message, "test/remaining")
        })
        .must_be("closing the alias rechecks the remaining spelling");
    let actions = explicit_dangerous_actions(
        &mut session,
        &canonical_text,
        &refreshed["params"]["diagnostics"].clone(),
    );
    assert_eq!(actions.len(), 1, "the remaining target offers its fix");
    let action = &actions[0];
    assert_eq!(action["kind"], "quickfix.bend-lint.dangerous");
    assert!(action["disabled"].is_null(), "{action}");
    assert!(action["command"].is_null());
    assert_eq!(
        action["edit"]["documentChanges"],
        json!([{
            "textDocument":{"uri":canonical_text,"version":2},
            "edits":[{"range":range(0, 9, 10),"newText":""}]
        }]),
        "the fix targets the remaining URI and its own version"
    );
    session.finish();
}
