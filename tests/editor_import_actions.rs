#[cfg(unix)]
#[path = "support/lsp_client.rs"]
pub mod lsp_client;
#[cfg(unix)]
mod support;

#[cfg(unix)]
mod protocol {
    use super::{lsp_client::LspClient, support::Must};
    use bend2_lsp::analysis::LineIndex;
    use serde_json::{Value, json};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Command, Stdio},
    };
    use tempfile::{TempDir, tempdir};
    use url::Url;

    fn client(root: &Path) -> LspClient {
        let compiler = root.join("compiler-stub");
        fs::write(&compiler, "#!/bin/sh\ncase \"$1\" in\nversion) printf 'Bend test\\n';;\n--help) printf 'Bend\\nusage: bend <file> --check-only\\nbend base\\n';;\nbase) printf 'def builtin: U32\\n  1\\n';;\nesac\nexit 0\n").must_be("write compiler stub");
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .must_be("compiler permissions");
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
        command
            .current_dir(root)
            .stderr(Stdio::null())
            .env("PATH", root)
            .env("HOME", root)
            .env("BEND_LIB", root.join("library"))
            .env_remove("BEND2_LSP_TRACE")
            .env_remove("BEND2_LSP_COMPILER_METRICS_FILE");
        let mut client = LspClient::spawn(command);
        client.initialize(root);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({
                "settings":{"bend2-lsp":{"compilerPath":compiler}}
            }),
        );
        client
    }

    fn fixture(source: &str) -> (TempDir, LspClient, Url, Url) {
        let root = tempdir().must_be("temporary workspace");
        let main = root.path().join("main.bend");
        let dep = root.path().join("dep.bend");
        fs::write(&main, source).must_be("main source");
        fs::write(&dep, "def value: U32\n  1\n").must_be("dependency source");
        let uri = Url::from_file_path(main).must_be("main URI");
        let dep_uri = Url::from_file_path(dep).must_be("dependency URI");
        let mut client = client(root.path());
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,"languageId":"bend","version":1,"text":source
            }}),
        );
        (root, client, uri, dep_uri)
    }

    fn change(client: &mut LspClient, uri: &Url, source: &str, version: i32) {
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":uri,"version":version}, "contentChanges":[{"text":source}]
            }),
        );
    }

    fn position(source: &str, needle: &str) -> Value {
        let offset = source.find(needle).must_be("reference in source");
        let (line, character) = LineIndex::new(source).position(source, offset);
        json!({"line":line,"character":character})
    }

    fn actions(
        client: &mut LspClient,
        uri: &Url,
        source: &str,
        needle: &str,
        only: &str,
    ) -> Vec<Value> {
        let position = position(source, needle);
        let response = client.request(
            "textDocument/codeAction",
            json!({
                "textDocument":{"uri":uri}, "range":{"start":position,"end":position},
                "context":{"diagnostics":[],"only":[only]}
            }),
        );
        response["result"]
            .as_array()
            .must_be("code action list")
            .clone()
    }

    fn apply(source: &str, action: &Value, uri: &Url, version: i32) -> String {
        let document = &action["edit"]["documentChanges"][0];
        assert_eq!(document["textDocument"]["uri"], json!(uri));
        assert_eq!(document["textDocument"]["version"], version);
        let index = LineIndex::new(source);
        let mut edits: Vec<_> = document["edits"]
            .as_array()
            .must_be("versioned edits")
            .iter()
            .map(|edit| {
                let start = &edit["range"]["start"];
                let end = &edit["range"]["end"];
                let offset = |position: &Value| {
                    index.offset(
                        source,
                        u32::try_from(position["line"].as_u64().must_be("line"))
                            .must_be("line fits"),
                        u32::try_from(position["character"].as_u64().must_be("character"))
                            .must_be("character fits"),
                    )
                };
                (
                    offset(start),
                    offset(end),
                    edit["newText"].as_str().must_be("edit text"),
                )
            })
            .collect();
        edits.sort_by_key(|(start, end, _)| (*start, *end));
        let mut result = source.to_owned();
        for (start, end, text) in edits.into_iter().rev() {
            result.replace_range(start..end, text);
        }
        result
    }

    fn definition(client: &mut LspClient, uri: &Url, source: &str, name: &str) -> Value {
        client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":uri},"position":position(source, name)
            }),
        )["result"]
            .clone()
    }

    #[test]
    fn auto_import_reuses_indexed_cached_package_path() {
        let root = tempdir().must_be("package workspace");
        let package = "0x0123456789abcdef0123456789abcdef";
        let directory = root.path().join("library").join(package);
        fs::create_dir_all(&directory).must_be("cached package directory");
        let target = directory.join("library.bend");
        fs::write(&target, "def cached: U32\n  1\n").must_be("cached package source");
        let mut client = client(root.path());
        let seed = Url::from_file_path(root.path().join("seed.bend")).must_be("seed URI");
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":seed,"languageId":"bend","version":1,
                "text":format!("import {package}/library.bend as Cached\n")
            }}),
        );
        // Availability queries use committed indexes, not unrelated pending builds.
        client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":seed}}),
        );
        let target_uri = Url::from_file_path(&target).must_be("cached URI");
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":target_uri,"languageId":"bend","version":1,"text":"def cached: U32\n  1\n"
            }}),
        );
        client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":target_uri}}),
        );
        let uri = Url::from_file_path(root.path().join("main.bend")).must_be("main URI");
        let source = "def main: U32\n  cached\n";
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,"languageId":"bend","version":1,"text":source
            }}),
        );
        let offered = actions(&mut client, &uri, source, "cached", "quickfix");
        assert_eq!(offered.len(), 1);
        let updated = apply(source, &offered[0], &uri, 1);
        assert!(updated.contains(&format!("import {package}/library.bend as ")));
        change(&mut client, &uri, &updated, 2);
        assert_eq!(
            definition(&mut client, &uri, &updated, "cached")["uri"],
            json!(Url::from_file_path(target).must_be("cached URI"))
        );
        client.finish();
    }

    #[test]
    fn auto_import_uses_latest_utf16_buffer_and_avoids_scope_collisions() {
        let disk = "def main: U32\n  stale\n";
        let (_root, mut client, uri, dep_uri) = fixture(disk);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":dep_uri,"languageId":"bend","version":1,"text":"def value: U32\n  1\n"
            }}),
        );
        client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":dep_uri}}),
        );
        let source = "# editor header 😀\ndef main(Dep: U32):\n  (\"😀\", value)\n";
        change(&mut client, &uri, source, 7);
        let offered = actions(&mut client, &uri, source, "value", "quickfix");
        assert_eq!(
            offered.len(),
            1,
            "available exact indexed symbol: {offered:?}"
        );
        let edited = apply(source, &offered[0], &uri, 7);
        assert!(edited.starts_with("# editor header 😀\nimport "));
        assert!(
            edited.contains(" as Dep2\n"),
            "avoid visible Dep binding: {edited}"
        );
        assert!(
            edited.contains("(\"😀\", Dep2.value)"),
            "UTF-16 replacement: {edited}"
        );
        assert!(!edited.contains("stale"));
        change(&mut client, &uri, &edited, 8);
        let target = definition(&mut client, &uri, &edited, "value");
        assert_eq!(target["uri"], json!(dep_uri));
        assert_eq!(target["range"]["start"], json!({"line":0,"character":4}));
        assert!(actions(&mut client, &uri, &edited, "value", "quickfix").is_empty());
        client.finish();
    }

    #[test]
    fn auto_import_reuses_existing_alias_without_duplicate_imports() {
        let source = "import ./dep.bend as Existing # preserve this\ndef main: U32\n  value\n";
        let (_root, mut client, uri, dep_uri) = fixture(source);
        let offered = actions(&mut client, &uri, source, "value", "quickfix");
        assert_eq!(offered.len(), 1);
        let edited = apply(source, &offered[0], &uri, 1);
        assert_eq!(edited, source.replace("  value", "  Existing.value"));
        change(&mut client, &uri, &edited, 2);
        assert_eq!(
            definition(&mut client, &uri, &edited, "value")["uri"],
            json!(dep_uri)
        );
        let shadowed =
            "import ./dep.bend as Existing # preserve this\ndef main(Existing: U32):\n  value\n";
        change(&mut client, &uri, shadowed, 3);
        assert!(actions(&mut client, &uri, shadowed, "value", "quickfix").is_empty());
        let unaliased = "import ./dep.bend\ndef main: U32\n  value\n";
        change(&mut client, &uri, unaliased, 4);
        assert!(actions(&mut client, &uri, unaliased, "value", "quickfix").is_empty());
        client.finish();
    }

    #[test]
    fn auto_import_reads_unsaved_target_symbols_and_ignores_unavailable_names() {
        let source = "def main: U32\n  fresh\n";
        let (_root, mut client, uri, dep_uri) = fixture(source);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":dep_uri,"languageId":"bend","version":3,"text":"def fresh: U32\n  2\n"
            }}),
        );
        client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":dep_uri}}),
        );
        let offered = actions(&mut client, &uri, source, "fresh", "quickfix");
        assert_eq!(offered.len(), 1);
        let edited = apply(source, &offered[0], &uri, 1);
        change(&mut client, &uri, &edited, 2);
        assert_eq!(
            definition(&mut client, &uri, &edited, "fresh")["uri"],
            json!(dep_uri)
        );
        let missing = "def main: U32\n  value\n";
        change(&mut client, &uri, missing, 3);
        assert!(actions(&mut client, &uri, missing, "value", "quickfix").is_empty());
        client.finish();
    }

    #[test]
    fn auto_import_reuses_only_cached_base_and_preserves_generated_provenance() {
        let source = "import Base\ndef main: U32\n  builtin\n";
        let (root, mut client, uri, _dep_uri) = fixture(source);
        let original = definition(&mut client, &uri, source, "builtin");
        let generated = original["uri"].as_str().must_be("compiler Base URI");
        assert!(
            !generated.starts_with(
                Url::from_directory_path(root.path())
                    .must_be("root URI")
                    .as_str()
            )
        );
        let unsaved = "def main: U32\n  builtin\n";
        change(&mut client, &uri, unsaved, 2);
        let offered = actions(&mut client, &uri, unsaved, "builtin", "quickfix");
        assert_eq!(offered.len(), 1);
        let edited = apply(unsaved, &offered[0], &uri, 2);
        assert!(edited.starts_with("import Base\n"));
        assert!(edited.contains("  builtin\n"));
        change(&mut client, &uri, &edited, 3);
        assert_eq!(
            definition(&mut client, &uri, &edited, "builtin")["uri"],
            generated
        );
        assert!(actions(&mut client, &uri, &edited, "builtin", "quickfix").is_empty());
        let local_shadow = "def main(builtin: U32):\n  Unknown.builtin\n";
        change(&mut client, &uri, local_shadow, 4);
        assert!(actions(&mut client, &uri, local_shadow, "builtin\n", "quickfix").is_empty());
        let global_shadow = "def builtin: U32\n  1\ndef main: U32\n  Unknown.builtin\n";
        change(&mut client, &uri, global_shadow, 5);
        assert!(actions(&mut client, &uri, global_shadow, "builtin\n", "quickfix").is_empty());
        client.finish();
    }

    #[test]
    fn organize_imports_preserves_comments_aliases_resolution_and_is_idempotent() {
        let source = "# header 😀\nimport ./z.bend as Z # z comment\n# dependency comment\nimport ./dep.bend as D # first\nimport dep.bend as D # duplicate comment\nimport ./dep.bend as Other\ndef main: U32\n  D.value\n";
        let (root, mut client, uri, dep_uri) = fixture(source);
        fs::write(root.path().join("z.bend"), "def other: U32\n  0\n").must_be("second module");
        let offered = actions(&mut client, &uri, source, "D.value", "source");
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0]["kind"], "source.organizeImports");
        let edited = apply(source, &offered[0], &uri, 1);
        assert!(edited.starts_with("# header 😀\n# duplicate comment\n# dependency comment\nimport ./dep.bend as D # first\n"), "attached comments: {edited}");
        assert_eq!(edited.matches("import ./dep.bend as D").count(), 1);
        assert!(
            !edited.contains("import dep.bend as D"),
            "equivalent indexed target is deduplicated"
        );
        assert!(
            edited.contains("import ./dep.bend as Other\n"),
            "distinct alias remains"
        );
        assert!(edited.contains("import ./z.bend as Z # z comment\n"));
        change(&mut client, &uri, &edited, 2);
        assert_eq!(
            definition(&mut client, &uri, &edited, "value")["uri"],
            json!(dep_uri)
        );
        assert!(
            actions(
                &mut client,
                &uri,
                &edited,
                "D.value",
                "source.organizeImports"
            )
            .is_empty()
        );
        client.finish();
    }

    #[test]
    fn organize_imports_keeps_conflicting_alias_precedence_and_filter_hierarchy() {
        let source = "import ./z.bend as Same\nimport ./dep.bend as Same\nimport ./dep.bend as Same\ndef main: U32\n  (\n";
        let (root, mut client, uri, _dep_uri) = fixture(source);
        fs::write(root.path().join("z.bend"), "def value: U32\n  9\n")
            .must_be("first alias target");
        let organized = actions(&mut client, &uri, source, "  (", "source");
        assert_eq!(organized.len(), 1);
        let edited = apply(source, &organized[0], &uri, 1);
        assert!(edited.starts_with("import ./z.bend as Same\nimport ./dep.bend as Same\ndef main"));
        let position = position(source, "(");
        let diagnostic = json!({"range":{"start":position,"end":position},
            "message":"Unclosed '(' delimiter", "code":"parsing"});
        let request = |only: &str| {
            json!({"textDocument":{"uri":uri},
            "range":{"start":position,"end":position},
            "context":{"diagnostics":[diagnostic],"only":[only]}})
        };
        let quick = client.request("textDocument/codeAction", request("quickfix"));
        let quick = quick["result"].as_array().must_be("quickfix list");
        assert_eq!(quick.len(), 1);
        assert!(apply(source, &quick[0], &uri, 1).contains("()"));
        let unrelated = client.request("textDocument/codeAction", request("quickfix.compiler"));
        assert_eq!(unrelated["result"], json!([]));
        let all = client.request("textDocument/codeAction", request(""));
        let kinds: Vec<_> = all["result"]
            .as_array()
            .must_be("all actions")
            .iter()
            .map(|action| action["kind"].as_str().must_be("kind"))
            .collect();
        assert_eq!(kinds, ["source.organizeImports", "quickfix"]);
        client.finish();
    }

    #[test]
    fn organize_imports_uses_unsaved_whole_document_without_diagnostics() {
        let saved = "def main: U32\n  0\n";
        let (root, mut client, uri, _dep_uri) = fixture(saved);
        fs::write(root.path().join("z.bend"), "def other: U32\n  0\n").must_be("second module");
        for (version, newline) in [(2, "\n"), (4, "\r\n")] {
            let unsaved = "# file header 😀\nimport ./z.bend as Z # z comment\n# dependency comment\nimport ./dep.bend as D\nimport dep.bend as D # duplicate comment\n\nimport ./z.bend as LaterZ\n# later dependency\nimport ./dep.bend as LaterD\n\ndef main: U32\n  D.value # unsaved body 😀  \n"
                .replace('\n', newline);
            let expected = "# file header 😀\n# duplicate comment\n# dependency comment\nimport ./dep.bend as D\nimport ./z.bend as Z # z comment\n\n# later dependency\nimport ./dep.bend as LaterD\nimport ./z.bend as LaterZ\n\ndef main: U32\n  D.value # unsaved body 😀  \n"
                .replace('\n', newline);
            change(&mut client, &uri, &unsaved, version);
            let (line, character) = LineIndex::new(&unsaved).position(&unsaved, unsaved.len());
            let end = json!({"line":line,"character":character});
            let response = client.request(
                "textDocument/codeAction",
                json!({
                    "textDocument":{"uri":uri},
                    "range":{"start":{"line":0,"character":0},"end":end},
                    "context":{"diagnostics":[],"only":["source.organizeImports"]}
                }),
            );
            let offered = response["result"].as_array().must_be("source actions");
            assert_eq!(offered.len(), 1);
            let edited = apply(&unsaved, &offered[0], &uri, version);
            assert_eq!(edited, expected);
            assert_eq!(
                fs::read_to_string(root.path().join("main.bend")).must_be("saved source"),
                saved,
                "organizing the unsaved buffer must not write the file"
            );
            change(&mut client, &uri, &edited, version + 1);
            assert!(
                actions(
                    &mut client,
                    &uri,
                    &edited,
                    "  D.value",
                    "source.organizeImports"
                )
                .is_empty()
            );
        }
        client.finish();
    }

    #[test]
    fn organize_imports_preserves_unaliased_precedence_and_incomplete_imports() {
        let source = "import ./z.bend\nimport ./dep.bend\nimport ./dep.bend\n\nimport ./z.bend as Pending extra\nimport ./z.bend as Z\n# valid dependency\nimport ./dep.bend as D\n\ndef main: U32\n  value\n";
        let (root, mut client, uri, _dep_uri) = fixture(source);
        fs::write(root.path().join("z.bend"), "def value: U32\n  9\n")
            .must_be("first unaliased target");
        let offered = actions(
            &mut client,
            &uri,
            source,
            "  value",
            "source.organizeImports",
        );
        assert_eq!(offered.len(), 1);
        let edited = apply(source, &offered[0], &uri, 1);
        assert_eq!(
            edited,
            "import ./z.bend\nimport ./dep.bend\n\nimport ./z.bend as Pending extra\n# valid dependency\nimport ./dep.bend as D\nimport ./z.bend as Z\n\ndef main: U32\n  value\n"
        );
        change(&mut client, &uri, &edited, 2);
        assert!(
            actions(
                &mut client,
                &uri,
                &edited,
                "  value",
                "source.organizeImports"
            )
            .is_empty()
        );
        client.finish();
    }

    #[test]
    fn organize_imports_preserves_crlf_and_missing_final_newline() {
        let source = "import ./z.bend as Z\r\nimport ./dep.bend as D # first\r\nimport dep.bend as D # duplicate";
        let (root, mut client, uri, dep_uri) = fixture(source);
        fs::write(root.path().join("z.bend"), "def other: U32\n  0\n").must_be("second module");
        let offered = actions(
            &mut client,
            &uri,
            source,
            "import",
            "source.organizeImports",
        );
        assert_eq!(offered.len(), 1);
        let edited = apply(source, &offered[0], &uri, 1);
        assert_eq!(
            edited,
            "# duplicate\r\nimport ./dep.bend as D # first\r\nimport ./z.bend as Z"
        );
        change(&mut client, &uri, &edited, 2);
        assert_eq!(
            definition(&mut client, &uri, &edited, "D # first")["uri"],
            json!(dep_uri)
        );
        assert!(
            actions(
                &mut client,
                &uri,
                &edited,
                "import",
                "source.organizeImports"
            )
            .is_empty()
        );
        client.finish();
    }
}
