use super::*;

#[test]
fn saving_another_entry_preserves_the_previous_documents_fix() {
    let mut session = Session::new(SOURCE, &[("other.bend", SOURCE)]);
    fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR)
        .must_be("make compiler publication an observable readiness barrier");
    let other = session.project().join("other.bend");
    let other_uri = uri(&other);
    let mut original = finding(&session.root, "test/original", range(0, 9, 10));
    original["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    // Each target keeps its own report. A project-scoped refresh of the first
    // entry must return its own finding again, not the sibling target's.
    let responses = session.control.join("responses");
    fs::create_dir(&responses).must_be("create per-target responses");
    target_response(&responses.join("main.bend.json"), vec![original], true);
    target_response(
        &responses.join("other.bend.json"),
        vec![finding(&other, "test/other", range(0, 9, 10))],
        true,
    );
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":other_uri,"languageId":"bend","version":1,"text":SOURCE
        }}),
    );
    session.compiler(1);
    session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == other_uri
                && message["params"]["version"] == 1
        })
        .must_be("second document is indexed before either saved check");
    session.save();
    let original_diagnostics = session.lint()["params"]["diagnostics"].clone();
    session.client.notify(
        "textDocument/didSave",
        json!({"textDocument":{"uri":other_uri}}),
    );
    session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == other_uri
                && message["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["code"] == "test/other"))
        })
        .must_be("second entry finishes its own analysis");
    let actions = session.actions(original_diagnostics, 1);
    assert_eq!(
        actions.len(),
        1,
        "saving an unchanged independent entry must not erase the previous fix"
    );
    assert_eq!(
        actions[0]["edit"]["documentChanges"],
        json!([{
            "textDocument":{"uri":session.document,"version":1},
            "edits":[{"range":range(0, 9, 10),"newText":""}]
        }])
    );
    session.finish();
}

#[test]
fn an_unsaved_document_in_another_project_does_not_erase_or_block_saved_checks() {
    let mut session = Session::new(SOURCE, &[]);
    let mut original = finding(&session.root, "test/original", range(0, 9, 10));
    original["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![original], vec![], true);
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    let other_project = session.directory.path().join("another project");
    fs::create_dir(&other_project).must_be("create independent project");
    fs::write(other_project.join("bend-lint.json"), "{\"rules\":{}}")
        .must_be("write independent project configuration");
    let other = other_project.join("other.bend");
    fs::write(&other, SOURCE).must_be("write independent saved file");
    let other_uri = uri(&other);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":other_uri,"languageId":"bend","version":1,
            "text":format!("{SOURCE}# unsaved change\n")
        }}),
    );
    session.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":other_uri}}),
    );
    let actions = session.actions(diagnostics, 1);
    assert_eq!(
        actions.len(),
        1,
        "an unsaved file outside captured project inputs must not erase a current fix"
    );
    assert_eq!(
        actions[0]["edit"]["documentChanges"][0]["textDocument"],
        json!({"uri":session.document,"version":1})
    );
    session.save();
    let packet = session.lint();
    assert!(
        packet["params"]["diagnostics"]
            .as_array()
            .must_be("diagnostics")
            .iter()
            .any(|item| item["code"] == "test/original")
    );
    session.finish();
}

#[test]
fn an_empty_entry_report_clears_an_older_imported_lint_finding() {
    let mut session = Session::new(SOURCE, &[("other.bend", SOURCE)]);
    fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR)
        .must_be("independent compiler diagnostic");
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.compiler(1);
    let other_uri = uri(&session.project().join("other.bend"));
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":other_uri,"languageId":"bend","version":1,"text":SOURCE
        }}),
    );
    session.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":other_uri}}),
    );
    // Phase one: the sibling entry reports an imported finding for the root.
    let responses = session.control.join("responses");
    fs::create_dir(&responses).must_be("create per-target responses");
    target_response(
        &responses.join("other.bend.json"),
        vec![finding(&session.root, "test/imported", range(0, 9, 10))],
        true,
    );
    session.client.notify(
        "textDocument/didSave",
        json!({"textDocument":{"uri":other_uri}}),
    );
    session.lint();
    // Drain any earlier empty root publication so the observed clear below is
    // the phase-two refresh, not a raced interim packet.
    while session
        .client
        .receive_matching(Duration::from_millis(150), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && !has_lint(message)
        })
        .is_some()
    {}
    // Phase two: both entries report cleanly, so the project-scoped refresh of
    // either entry cannot resurrect the older imported finding.
    target_response(&responses.join("main.bend.json"), vec![], true);
    target_response(&responses.join("other.bend.json"), vec![], true);
    session.save();
    let cleared = session.client.receive_matching(TIMEOUT, |message| {
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"] == session.document
            && message["params"]["version"] == 1
            && message["params"]["diagnostics"]
                .as_array()
                .is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item["message"] == "selected compiler rejection")
                        && items.iter().all(|item| item["source"] != "bend-lint")
                })
    });
    assert!(
        cleared.is_some(),
        "a successful empty report must clear the old imported lint finding without clearing the compiler diagnostic"
    );
    session.finish();
}

#[cfg(unix)]
#[test]
fn opening_a_dirty_new_symlink_alias_invalidates_a_captured_input() {
    let mut session = Session::new(SOURCE, &[("dep.bend", SOURCE)]);
    let mut original = finding(&session.root, "test/original", range(0, 9, 10));
    original["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    session.response(vec![original], vec![], true);
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.save();
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    let alias = session.directory.path().join("new alias.bend");
    std::os::unix::fs::symlink(session.project().join("dep.bend"), &alias)
        .must_be("create a previously unseen external source alias");
    let alias_uri = uri(&alias);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":alias_uri,"languageId":"bend","version":1,
            "text":format!("{SOURCE}# unsaved dependency\n")
        }}),
    );
    session.client.request(
        "textDocument/documentSymbol",
        json!({"textDocument":{"uri":alias_uri}}),
    );
    assert!(
        session.actions(diagnostics, 1).is_empty(),
        "a dirty alias of a captured dependency must fence dangerous edits to the root"
    );
    session.finish();
}

#[test]
fn a_disk_only_input_change_finishes_with_an_actionable_skipped_status() {
    let mut session = Session::new(SOURCE, &[("dep.bend", SOURCE)]);
    session.standard_response();
    session.block();
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    session.save();
    session.wait_file("address");
    fs::write(
        session.project().join("dep.bend"),
        format!("{SOURCE}# external edit\n"),
    )
    .must_be("change a captured input without a watched-file notification");
    session.release();
    let skipped = session.client.receive_matching(TIMEOUT, |message| {
        message["method"] == "window/showMessage" && message["params"]["type"] == 2
    });
    assert!(
        skipped.is_some(),
        "discarding a finished check must explain why its results were skipped"
    );
    let status = session.client.request(
        "workspace/executeCommand",
        json!({
            "command":"bend2.lint.status","arguments":[{"uri":session.document}]
        }),
    );
    assert_eq!(
        status["result"]["state"], "skipped",
        "a finished child must not remain running"
    );
    assert!(
        status["result"]["reason"]
            .as_str()
            .must_be("skip reason")
            .contains("next check"),
        "the reason explains that a later check re-reads the inputs: {status}"
    );
    session.no_lint();
    session.finish();
}

#[test]
fn previously_missing_external_imports_fence_old_fixes() {
    for created_on_disk in [false, true] {
        let source = format!("import ../external.bend as External\n{SOURCE}");
        let mut session = Session::new(&source, &[]);
        let mut original = finding(&session.root, "test/original", range(1, 9, 10));
        original["fixes"] = json!([fix(&session.root, range(1, 9, 10), "")]);
        session.response(vec![original], vec![], true);
        session.configure(true, true, 10_000);
        session.open(&source);
        session.save();
        let diagnostics = session.lint()["params"]["diagnostics"].clone();
        let params = json!({
            "textDocument":{"uri":session.document},"range":range(1, 9, 10),
            "context":{"diagnostics":diagnostics,"triggerKind":1,"only":["quickfix.bend-lint.dangerous"]}
        });
        let before = session
            .client
            .request("textDocument/codeAction", params.clone());
        assert_eq!(
            before["result"].as_array().must_be("initial fixes").len(),
            1
        );
        let external = session
            .project()
            .parent()
            .must_be("project parent")
            .join("external.bend");
        if created_on_disk {
            fs::write(&external, SOURCE)
                .must_be("create a previously missing dependency without a notification");
        } else {
            let external_uri = uri(&external);
            session.client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":external_uri,"languageId":"bend","version":1,"text":SOURCE
                }}),
            );
            session.client.request(
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":external_uri}}),
            );
        }
        let after = session.client.request("textDocument/codeAction", params);
        assert!(
            after["result"]
                .as_array()
                .must_be("current fixes")
                .is_empty(),
            "a newly available dependency invalidates the captured missing-input assumption (disk={created_on_disk})"
        );
        session.finish();
    }
}

fn target_response(path: &Path, findings: Vec<Value>, ok: bool) {
    let response = json!({"ok":ok,"findings":findings,"suppressed":[],"skipped":[]});
    fs::write(
        path,
        serde_json::to_vec(&response).must_be("serialize target-specific response"),
    )
    .must_be("write target-specific response");
}

#[test]
fn editing_an_independent_open_document_refreshes_a_fenced_result() {
    let mut session = Session::new(SOURCE, &[("other.bend", SOURCE)]);
    let other = session.project().join("other.bend");
    let other_uri = uri(&other);
    let other_source = SOURCE;
    let mut original = finding(&session.root, "test/original", range(0, 9, 10));
    original["fixes"] = json!([fix(&session.root, range(0, 9, 10), "")]);
    // Each target has its own canned report: a check of the independent entry
    // must never fabricate findings for the retained document.
    let responses = session.control.join("responses");
    fs::create_dir(&responses).must_be("create per-target responses");
    target_response(&responses.join("main.bend.json"), vec![original], true);
    target_response(&responses.join("other.bend.json"), vec![], true);
    session.configure(true, true, 10_000);
    session.open(SOURCE);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":other_uri,"languageId":"bend","version":1,"text":other_source
        }}),
    );
    // Let the debounced open checks finish, then check the retained document
    // explicitly: its immutable project snapshot now includes both open inputs.
    thread::sleep(Duration::from_millis(700));
    while session
        .client
        .receive_matching(Duration::from_millis(150), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .is_some()
    {}
    let run = session.client.request_response(
        "workspace/executeCommand",
        json!({"command":"bend2.lint.run","arguments":[{"uri":session.document}]}),
    );
    assert!(run.get("error").is_none(), "{run}");
    let baseline = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .must_be("the retained document publishes its actionable result");
    let diagnostics = baseline["params"]["diagnostics"].clone();
    assert_eq!(
        session.actions(diagnostics, 1).len(),
        1,
        "the retained result offers its versioned dangerous fix"
    );
    while session
        .client
        .receive_matching(Duration::from_millis(150), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .is_some()
    {}
    // Edit only the independent document. The retained snapshot fenced this
    // document's revision too, so its visible result must be rechecked rather
    // than silently left stale with unavailable fixes.
    session.client.notify(
        "textDocument/didChange",
        json!({"textDocument":{"uri":other_uri,"version":2},
        "contentChanges":[{"text":format!("# other edit\n{other_source}")}]}),
    );
    session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == other_uri
                && message["params"]["version"] == 2
        })
        .must_be("the edited independent document is checked");
    let refreshed = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .must_be("editing an independent open document must refresh a fenced visible result");
    assert_eq!(
        session
            .actions(refreshed["params"]["diagnostics"].clone(), 1)
            .len(),
        1,
        "the refreshed result keeps its versioned dangerous fix available"
    );
    assert_eq!(
        fs::read_to_string(&session.root).must_be("unchanged retained document"),
        SOURCE
    );
    assert_eq!(
        fs::read_to_string(&other).must_be("unsaved independent edit stays off disk"),
        SOURCE
    );
    session.finish();
}

#[test]
fn closing_an_unrelated_buffer_during_preparation_reschedules_the_pending_entry() {
    let mut session = Session::new(SOURCE, &[("other.bend", SOURCE)]);
    let other_uri = uri(&session.project().join("other.bend"));
    fs::write(session.control.join("prepare-block"), "").must_be("block preparation");
    session.standard_response();
    // Open the unrelated buffer while analysis is disabled, so no check starts
    // for it and the only pending work belongs to the retained entry.
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":other_uri,"languageId":"bend","version":1,"text":SOURCE
        }}),
    );
    session.configure(true, false, 10_000);
    session.open(SOURCE);
    let prepare_address = session.wait_file("prepare-address");
    assert_eq!(session.prepares(), 1);
    assert_eq!(session.calls(), 0, "the entry waits for preparation");
    // Closing the unrelated buffer must not abort the pending project work.
    session.client.notify(
        "textDocument/didClose",
        json!({"textDocument":{"uri":other_uri}}),
    );
    fs::write(session.control.join("prepare-release"), "").must_be("release preparation");
    let packet = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .must_be("an unrelated close must not lose the pending entry's check");
    assert_eq!(packet["params"]["version"], 1);
    assert!(session.calls() >= 1);
    let arguments = session.wait_file("arguments");
    assert!(
        arguments
            .lines()
            .next()
            .is_some_and(|target| target.contains("main.bend")),
        "only the retained entry is checked: {arguments}"
    );
    socket_closed(&prepare_address);
    session.finish();
}

#[cfg(unix)]
#[test]
fn retargeting_an_external_import_symlink_fences_and_rechecks_the_retained_fix() {
    use std::os::unix::fs::symlink;

    let source = "import ../ext/link.bend as Ext\nimport ../ext/same.bend as Same\n\ndef main(+x: U32) -> U32:\n  x\n";
    let mut session = Session::new(source, &[]);
    let ext = session.directory.path().join("ext");
    fs::create_dir(&ext).must_be("create external import directory");
    let first = ext.join("real1.bend");
    let second = ext.join("real2.bend");
    let link = ext.join("link.bend");
    let bytes = "import Base\n\ndef value() -> U32:\n  1\n";
    fs::write(&first, bytes).must_be("write original external target");
    fs::write(&second, bytes).must_be("write replacement external target");
    symlink(&first, &link).must_be("create external import alias");
    symlink(&first, ext.join("same.bend")).must_be("create second alias of the same source");
    let mut original = finding(&session.root, "test/original", range(3, 9, 10));
    original["fixes"] = json!([fix(&session.root, range(3, 9, 10), "")]);
    session.response(vec![original], vec![], true);
    session.configure(true, true, 10_000);
    session.open(source);
    let diagnostics = session.lint()["params"]["diagnostics"].clone();
    assert_eq!(
        session
            .actions_at(diagnostics.clone(), 1, range(3, 0, 24))
            .len(),
        1,
        "the retained external-import result offers its versioned fix"
    );
    // Atomically replace the alias with an identical-content target. The
    // captured input identity changed even though its bytes did not.
    let replacement = ext.join("link.next");
    symlink(&second, &replacement).must_be("create replacement alias");
    fs::rename(&replacement, &link).must_be("atomically retarget the external alias");
    assert!(
        session
            .actions_at(diagnostics, 1, range(3, 0, 24))
            .is_empty(),
        "retargeting a captured external alias must fence the stale fix"
    );
    while session
        .client
        .receive_matching(Duration::from_millis(150), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
        })
        .is_some()
    {}
    let calls = session.calls();
    let mut current = finding(&session.root, "test/retargeted", range(3, 9, 10));
    current["fixes"] = json!([fix(&session.root, range(3, 9, 10), "")]);
    session.response(vec![current], vec![], true);
    session.client.notify(
        "textDocument/didOpen",
        json!({"textDocument":{
            "uri":uri(&link),"languageId":"bend","version":1,"text":bytes
        }}),
    );
    let refreshed = session
        .client
        .receive_matching(TIMEOUT, |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == session.document
                && has_lint(message)
                && message["params"]["diagnostics"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["code"] == "test/retargeted"))
        })
        .must_be("the retargeted external import is rechecked");
    assert!(session.calls() > calls, "the recheck runs the real process");
    assert_eq!(
        session
            .actions_at(
                refreshed["params"]["diagnostics"].clone(),
                1,
                range(3, 0, 24)
            )
            .len(),
        1,
        "the fresh result over the new alias offers its fix again"
    );
    assert_eq!(
        fs::read_to_string(&link).must_be("alias resolves to identical bytes"),
        bytes
    );
    assert_eq!(
        fs::read_to_string(&first).must_be("original target unchanged"),
        bytes
    );
    session.finish();
}
