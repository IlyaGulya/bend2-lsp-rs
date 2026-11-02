use super::*;

// A compiler check uses the requested editor spelling's own buffer, while the
// physical file keeps one representative text for its imports. A fresh
// compiler configuration rechecks every live spelling, so each current URI
// must publish its own compiler result at its own LSP version even when the
// other spelling is the representative one. The earlier publications are
// consumed first, so only the rechecked spellings can satisfy the assertions.
#[test]
fn equivalent_uri_spellings_publish_compiler_diagnostics_at_their_own_versions() {
    let mut session = Session::new(SOURCE, &[]);
    fs::write(session.control.join("compiler.stderr"), COMPILER_ERROR)
        .must_be("compiler error fixture");

    // Percent-encoded unreserved letters keep distinct URI strings while
    // decoding to one normalized lexical path.
    let canonical = session.document.clone();
    let alias = canonical.replacen("/main.bend", "/%6Dain.bend", 1);
    assert_ne!(alias, canonical, "the alias keeps a distinct URI string");
    assert_eq!(
        Url::parse(&alias)
            .must_be("alias document URI")
            .to_file_path()
            .must_be("alias path"),
        Url::parse(&canonical)
            .must_be("canonical document URI")
            .to_file_path()
            .must_be("canonical path"),
        "both spellings decode to one normalized lexical path"
    );

    let compiler_error = |message: &Value, text: &str, version: i32| {
        message["method"] == "textDocument/publishDiagnostics"
            && message["params"]["uri"].as_str() == Some(text)
            && message["params"]["version"] == version
            && message["params"]["diagnostics"]
                .as_array()
                .is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item["message"] == "selected compiler rejection")
                })
    };

    // Open the two spellings in sequence, waiting for each one's own compiler
    // result: the alias commits last and therefore supplies the representative
    // text that dependency nodes use, while both keep independent versions.
    for (text, version) in [(&canonical, 2), (&alias, 7)] {
        session.client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":text,"languageId":"bend","version":version,"text":SOURCE}}),
        );
        session
            .client
            .receive_matching(TIMEOUT, |message| compiler_error(message, text, version))
            .must_be("each spelling publishes its own compiler result when opened");
    }
    // Drop every publication captured so far; only the recheck below may
    // satisfy the assertions.
    while session.client.receive_matching(QUIET, |_| true).is_some() {}

    // Fresh compiler configuration rechecks every live spelling at its current
    // version, including the spelling that no longer owns the representative
    // analysis text.
    session.client.notify(
        "workspace/didChangeConfiguration",
        json!({"settings":{"bend2-lsp":{"compilerPath":executable()}}}),
    );

    session
        .client
        .receive_matching(TIMEOUT, |message| compiler_error(message, &canonical, 2))
        .must_be(
            "the nonrepresentative spelling publishes its own compiler result at its own version",
        );
    session
        .client
        .receive_matching(TIMEOUT, |message| compiler_error(message, &alias, 7))
        .must_be("the representative spelling publishes its compiler result at its own version");

    session.finish();
}
