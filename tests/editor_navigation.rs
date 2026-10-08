#[cfg(unix)]
#[path = "support/lsp_client.rs"]
pub mod lsp_client;
#[cfg(unix)]
mod support;

#[cfg(unix)]
mod navigation {
    use super::{lsp_client::LspClient, support::Must};
    use bend2_lsp::analysis::LineIndex;
    use serde_json::{Value, json};
    use std::{
        fmt::Write as _,
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Command, Stdio},
    };
    use url::Url;

    fn client(root: &Path) -> LspClient {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).must_be("create compiler directory");
        let bend = bin.join("bend");
        fs::write(&bend, "#!/bin/sh\nif [ \"$1\" = base ]; then printf 'def builtin() -> U32:\\n  1\\n'; fi\nexit 0\n")
            .must_be("write compiler fixture");
        fs::set_permissions(&bend, fs::Permissions::from_mode(0o755))
            .must_be("compiler permissions");
        let paths = std::iter::once(bin)
            .chain(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ))
            .collect::<Vec<_>>();
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
        command
            .env("PATH", std::env::join_paths(paths).must_be("test PATH"))
            .env("BEND_LIB", root.join("hub"))
            .stderr(Stdio::null());
        let mut client = LspClient::spawn(command);
        client.initialize(root);
        client
    }

    fn uri(path: &Path) -> String {
        Url::from_file_path(path).must_be("file URI").into()
    }

    fn open(client: &mut LspClient, uri: &str, version: i32, text: &str) {
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"bend","version":version,"text":text}}),
        );
    }

    fn change(client: &mut LspClient, uri: &str, version: i32, text: &str) {
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]}),
        );
    }

    fn position(source: &str, offset: usize) -> Value {
        let (line, character) = LineIndex::new(source).position(source, offset);
        json!({"line":line,"character":character})
    }

    fn rename(client: &mut LspClient, uri: &str, source: &str, offset: usize, name: &str) -> Value {
        client.request_response(
            "textDocument/rename",
            json!({"textDocument":{"uri":uri},"position":position(source,offset),"newName":name}),
        )
    }

    fn prepare(client: &mut LspClient, uri: &str, source: &str, offset: usize) -> Value {
        client.request_response(
            "textDocument/prepareRename",
            json!({"textDocument":{"uri":uri},"position":position(source,offset)}),
        )
    }

    fn preview(source: &str, start: usize, name: &str) -> Value {
        json!({
            "range":{"start":position(source,start),"end":position(source,start+name.len())},
            "placeholder":name
        })
    }

    fn apply(source: &str, edits: &Value) -> String {
        let index = LineIndex::new(source);
        let mut edits: Vec<_> = edits
            .as_array()
            .must_be("text edits")
            .iter()
            .map(|edit| {
                let point = |point: &Value| {
                    index.offset(
                        source,
                        u32::try_from(point["line"].as_u64().must_be("line")).must_be("line u32"),
                        u32::try_from(point["character"].as_u64().must_be("character"))
                            .must_be("character u32"),
                    )
                };
                (
                    point(&edit["range"]["start"]),
                    point(&edit["range"]["end"]),
                    edit["newText"].as_str().must_be("replacement"),
                )
            })
            .collect();
        edits.sort_unstable_by_key(|(start, _, _)| std::cmp::Reverse(*start));
        let mut text = source.to_owned();
        for (start, end, replacement) in edits {
            text.replace_range(start..end, replacement);
        }
        text
    }

    #[test]
    fn document_links_share_definition_targets_for_base_local_and_cached_packages() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        let hash = "0x0123456789abcdef0123456789abcdef";
        fs::create_dir_all(root.join("hub").join(hash)).must_be("cached package");
        fs::create_dir_all(root.join("hub/names")).must_be("package names");
        fs::write(root.join("hub/names/pkg@1.0.0.0"), hash).must_be("named package mapping");
        fs::write(
            root.join("hub").join(hash).join("dep.bend"),
            "def value = 1\n",
        )
        .must_be("package source");
        fs::write(root.join("dep.bend"), "def value = 2\n").must_be("local source");
        fs::write(root.join("module"), "def value = 3\n").must_be("extensionless module source");
        let source = "import Base # generated\nimport ./dep.bend as Local\nimport pkg@1.0.0.0/dep.bend as Package\nimport ./module as Module\nimport ./missing.bend as Missing\ndef main = Local.value\n";
        let main = uri(&root.join("main.bend"));
        let mut client = client(root);
        open(&mut client, &main, 1, source);
        let links = client.request(
            "textDocument/documentLink",
            json!({"textDocument":{"uri":main}}),
        );
        let links = links["result"].as_array().must_be("document links");
        assert_eq!(links.len(), 4, "unresolved imports must not get links");
        assert_eq!(links[1]["target"], uri(&root.join("dep.bend")));
        assert_eq!(
            links[2]["target"],
            uri(&root.join("hub").join(hash).join("dep.bend"))
        );
        assert_eq!(links[3]["target"], uri(&root.join("module")));
        for (line, link) in links.iter().enumerate() {
            let definition = client.request(
                "textDocument/definition",
                json!({"textDocument":{"uri":main},"position":{"line":line,"character":8}}),
            );
            assert_eq!(link["target"], definition["result"]["uri"]);
        }
        let base =
            Url::parse(links[0]["target"].as_str().must_be("Base target")).must_be("Base URL");
        assert_eq!(
            fs::read_to_string(base.to_file_path().must_be("Base path")).must_be("Base source"),
            "def builtin() -> U32:\n  1\n"
        );
        client.finish();
    }

    #[test]
    fn alias_rename_changes_declaration_and_unshadowed_roots_with_utf16_ranges() {
        let temp = tempfile::tempdir().must_be("workspace");
        fs::write(temp.path().join("dep.bend"), "def value = 1\n").must_be("module source");
        let main = uri(&temp.path().join("main.bend"));
        let source = "import ./dep.bend as Dep # keep\ndef main = (\"😀\", Dep.value)\ndef shadow(Dep: U32) -> U32:\n  Dep.value\n# Dep.value\n";
        let expected = "import ./dep.bend as Module # keep\ndef main = (\"😀\", Module.value)\ndef shadow(Dep: U32) -> U32:\n  Dep.value\n# Dep.value\n";
        let mut client = client(temp.path());
        open(&mut client, &main, 7, source);
        for offset in [
            source.find("Dep #").must_be("alias declaration"),
            source.find("Dep.value").must_be("qualified root"),
        ] {
            let response = rename(&mut client, &main, source, offset, "Module");
            assert!(response.get("error").is_none(), "{response}");
            let edits = &response["result"]["documentChanges"][0];
            assert_eq!(edits["textDocument"]["version"], 7);
            assert_eq!(apply(source, &edits["edits"]), expected);
        }
        client.finish();
    }

    #[test]
    fn import_path_cursor_positions_reject_symbol_rename_without_alias_or_path_edits() {
        let temp = tempfile::tempdir().must_be("workspace");
        let main = uri(&temp.path().join("main.bend"));
        let mut client = client(temp.path());
        // Each spelling collides with a path segment or the extension.
        for (version, path, alias) in [
            (1, "test.bend", "test"),
            (2, "./nested/test.bend", "nested"),
            (3, "./nested/test.bend", "test"),
            (4, "./nested/test.bend", "bend"),
            (5, "pkg@1.0.0.0/test.bend", "test"),
        ] {
            let source = format!("import {path} as {alias}\ndef main = {alias}.foo\n");
            if version == 1 {
                open(&mut client, &main, version, &source);
            } else {
                change(&mut client, &main, version, &source);
            }
            let start = "import ".len();
            for offset in start..=start + path.len() {
                for response in [
                    prepare(&mut client, &main, &source, offset),
                    rename(&mut client, &main, &source, offset, "Module"),
                ] {
                    assert_eq!(response["error"]["code"], -32602, "{offset}: {response}");
                    assert_eq!(
                        response["error"]["message"],
                        "Import paths cannot be renamed with Rename Symbol; rename the file or folder in the project tree instead"
                    );
                    assert!(response.get("result").is_none(), "{response}");
                }
            }
            let alias_start = source.find(" as ").must_be("alias") + " as ".len();
            for offset in alias_start..alias_start + alias.len() {
                assert_eq!(
                    prepare(&mut client, &main, &source, offset)["result"],
                    preview(&source, alias_start, alias)
                );
                let response = rename(&mut client, &main, &source, offset, "Module");
                let edit = &response["result"]["documentChanges"][0];
                assert_eq!(edit["textDocument"]["version"], version);
                assert_eq!(
                    apply(&source, &edit["edits"]),
                    format!("import {path} as Module\ndef main = Module.foo\n")
                );
            }
        }
        client.finish();
    }

    #[test]
    fn rename_preview_and_edits_follow_alias_member_and_shadowed_local_in_unsaved_snapshots() {
        let temp = tempfile::tempdir().must_be("workspace");
        let dependency = temp.path().join("test.bend");
        fs::write(&dependency, "def disk_only = 0\n").must_be("disk dependency");
        let dependency_uri = uri(&dependency);
        let main_path = temp.path().join("main.bend");
        fs::write(&main_path, "def disk_only = 0\n").must_be("disk consumer");
        let main = uri(&main_path);
        let source = "import test.bend as Test\nimport other.bend as Other\ndef main = (\"😀\", Test.foo, Other.foo)\ndef shadow(Test: U32) -> U32:\n  Test.foo\n# Test.foo\n";
        let dependency_source = "def foo = 1\ndef use = foo\n";
        let mut client = client(temp.path());
        open(&mut client, &dependency_uri, 3, dependency_source);
        open(&mut client, &main, 7, source);
        let declaration = source.find("Test\n").must_be("alias declaration");
        let qualifier = source.find("Test.foo").must_be("module qualifier");
        for start in [declaration, qualifier] {
            for offset in start..start + "Test".len() {
                assert_eq!(
                    prepare(&mut client, &main, source, offset)["result"],
                    preview(source, start, "Test")
                );
                let renamed = rename(&mut client, &main, source, offset, "Module");
                let edit = &renamed["result"]["documentChanges"][0];
                assert_eq!(edit["textDocument"]["version"], 7);
                assert_eq!(
                    apply(source, &edit["edits"]),
                    "import test.bend as Module\nimport other.bend as Other\ndef main = (\"😀\", Module.foo, Other.foo)\ndef shadow(Test: U32) -> U32:\n  Test.foo\n# Test.foo\n"
                );
            }
        }
        let member = qualifier + "Test.".len();
        for offset in member..member + "foo".len() {
            assert_eq!(
                prepare(&mut client, &main, source, offset)["result"],
                preview(source, member, "foo")
            );
            let renamed = rename(&mut client, &main, source, offset, "bar");
            let changes = renamed["result"]["changes"].as_object().must_be("symbol changes");
            assert_eq!(changes.len(), 2, "{renamed}");
            assert_eq!(
                apply(source, &changes[&main]),
                "import test.bend as Test\nimport other.bend as Other\ndef main = (\"😀\", Test.bar, Other.foo)\ndef shadow(Test: U32) -> U32:\n  Test.foo\n# Test.foo\n"
            );
            assert_eq!(
                apply(dependency_source, &changes[&dependency_uri]),
                "def bar = 1\ndef use = bar\n"
            );
        }
        let local_declaration = source.find("Test:").must_be("local declaration");
        let local_use = source.rfind("  Test.foo").must_be("shadowed qualifier") + 2;
        for start in [local_declaration, local_use] {
            for offset in start..start + "Test".len() {
                assert_eq!(
                    prepare(&mut client, &main, source, offset)["result"],
                    preview(source, start, "Test")
                );
                let renamed = rename(&mut client, &main, source, offset, "Local");
                assert_eq!(
                    apply(source, &renamed["result"]["changes"][&main]),
                    "import test.bend as Test\nimport other.bend as Other\ndef main = (\"😀\", Test.foo, Other.foo)\ndef shadow(Local: U32) -> U32:\n  Local.foo\n# Test.foo\n"
                );
            }
        }
        for offset in [
            qualifier + "Test".len(), // separator, not either rename target
            local_use + "Test.".len(), // unsupported member of a shadowed local
            source.find("Other.foo").must_be("unresolved qualifier") + "Other.".len(),
            source.rfind("Test.foo").must_be("comment"),
            source.find('😀').must_be("string"),
        ] {
            assert_eq!(prepare(&mut client, &main, source, offset)["result"], Value::Null);
            assert_eq!(
                rename(&mut client, &main, source, offset, "Changed")["result"],
                Value::Null
            );
        }
        client.finish();
    }

    #[test]
    fn alias_rename_rejects_import_global_and_local_capture_conflicts() {
        let temp = tempfile::tempdir().must_be("workspace");
        let main = uri(&temp.path().join("main.bend"));
        let source = "import ./dep.bend as Dep\nimport ./other.bend as Other\ndef Global = 1\ndef main(Capture: U32) -> U32:\n  Dep.value\n";
        let mut client = client(temp.path());
        open(&mut client, &main, 1, source);
        let offset = source.find("Dep\n").must_be("declaration");
        for name in ["Other", "Global", "Capture", "two.names", "import"] {
            let response = rename(&mut client, &main, source, offset, name);
            assert_eq!(response["error"]["code"], -32602, "{name}: {response}");
        }
        client.finish();
    }

    #[test]
    fn file_moves_edit_unsaved_and_closed_consumers_and_relocate_graph_identity() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        fs::create_dir_all(root.join("lib")).must_be("old modules");
        fs::create_dir_all(root.join("new")).must_be("new modules");
        let target = root.join("lib/dep.bend");
        let consumer = root.join("lib/consumer.bend");
        let new_target = root.join("new/renamed.bend");
        let new_consumer = root.join("new/consumer.bend");
        fs::write(&target, "def value = 1\n").must_be("target");
        let closed_source = "import ./dep.bend as Dep # 😀 keep alias\ndef exported = Dep.value\n";
        fs::write(&consumer, closed_source).must_be("closed consumer");
        let main_path = root.join("main.bend");
        fs::write(&main_path, "def disk_only = 0\n").must_be("disk differs from buffer");
        let main = uri(&main_path);
        let source = "import Base\nimport ./lib/dep.bend as Dep # unsaved\nimport ./lib/consumer.bend as Consumer\ndef main = Dep.value\n";
        let mut client = client(root);
        open(&mut client, &main, 9, source);
        let before = client.request(
            "textDocument/documentLink",
            json!({"textDocument":{"uri":main}}),
        );
        let base_uri = before["result"][0]["target"].clone();
        let files = json!({"files":[{"oldUri":uri(&target),"newUri":uri(&new_target)},{"oldUri":uri(&consumer),"newUri":uri(&new_consumer)}]});
        let response = client.request("workspace/willRenameFiles", files.clone());
        assert!(response.get("error").is_none(), "{response}");
        let changes = response["result"]["documentChanges"]
            .as_array()
            .must_be("file rename edits");
        assert_eq!(changes.len(), 2);
        let main_change = changes
            .iter()
            .find(|change| change["textDocument"]["uri"] == main)
            .must_be("unsaved consumer edit");
        assert_eq!(main_change["textDocument"]["version"], 9);
        let main_updated = apply(source, &main_change["edits"]);
        assert_eq!(
            main_updated,
            "import Base\nimport ./new/renamed.bend as Dep # unsaved\nimport ./new/consumer.bend as Consumer\ndef main = Dep.value\n"
        );
        let closed_change = changes
            .iter()
            .find(|change| change["textDocument"]["uri"] == uri(&consumer))
            .must_be("closed consumer edit");
        assert!(closed_change["textDocument"]["version"].is_null());
        let closed_updated = apply(closed_source, &closed_change["edits"]);
        assert_eq!(
            closed_updated,
            "import ./renamed.bend as Dep # 😀 keep alias\ndef exported = Dep.value\n"
        );
        fs::write(&consumer, &closed_updated).must_be("client applies closed edit");
        change(&mut client, &main, 10, &main_updated);
        fs::rename(&target, &new_target).must_be("client moves target");
        fs::rename(&consumer, &new_consumer).must_be("client moves importer");
        client.notify("workspace/didRenameFiles", files);
        let links = client.request(
            "textDocument/documentLink",
            json!({"textDocument":{"uri":main}}),
        );
        assert_eq!(
            links["result"][0]["target"], base_uri,
            "generated Base provenance is not relocated"
        );
        assert_eq!(links["result"][1]["target"], uri(&new_target));
        let references = client.request("textDocument/references", json!({"textDocument":{"uri":main},"position":{"line":3,"character":17},"context":{"includeDeclaration":false}}));
        assert!(
            references["result"]
                .as_array()
                .must_be("imported references")
                .iter()
                .any(|location| {
                    location["uri"] == uri(&new_consumer) && location["range"]["start"]["line"] == 1
                }),
            "closed indexed consumer must refresh its imports: {references}"
        );
        let old = client.request(
            "textDocument/documentLink",
            json!({"textDocument":{"uri":uri(&consumer)}}),
        );
        assert!(
            old["result"].is_null(),
            "retired URI must not retain a document: {old}"
        );
        assert_eq!(
            fs::read_to_string(&main_path).must_be("unchanged on-disk unsaved root"),
            "def disk_only = 0\n"
        );
        client.finish();
    }

    #[test]
    fn folder_move_reindexes_open_importer_and_preserves_unsaved_revision_ownership() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        fs::create_dir_all(root.join("src")).must_be("source folder");
        fs::create_dir_all(root.join("nested")).must_be("destination parent");
        fs::write(root.join("dé😀.bend"), "def value = 42\n").must_be("unchanged target");
        fs::write(root.join("src/sibling.bend"), "def sibling = 1\n").must_be("co-moved target");
        let old_path = root.join("src/main.bend");
        let new_path = root.join("nested/src/main.bend");
        let old_uri = uri(&old_path);
        let new_uri = uri(&new_path);
        let source = "import ../dé😀.bend as Dep # keep\nimport sibling.bend as Sibling # unchanged spelling\ndef main = Dep.value\n";
        fs::write(&old_path, "def disk_only = 0\n").must_be("disk source");
        let mut client = client(root);
        open(&mut client, &old_uri, 12, source);
        let files = json!({"files":[{"oldUri":uri(&root.join("src")),"newUri":uri(&root.join("nested/src"))}]});
        let response = client.request("workspace/willRenameFiles", files.clone());
        let edit = &response["result"]["documentChanges"][0];
        assert_eq!(edit["textDocument"]["uri"], old_uri);
        assert_eq!(edit["textDocument"]["version"], 12);
        let updated = apply(source, &edit["edits"]);
        assert_eq!(
            updated,
            "import ../../dé😀.bend as Dep # keep\nimport sibling.bend as Sibling # unchanged spelling\ndef main = Dep.value\n"
        );
        change(&mut client, &old_uri, 13, &updated);
        fs::rename(root.join("src"), root.join("nested/src")).must_be("client folder move");
        client.notify("workspace/didRenameFiles", files);
        let links = client.request(
            "textDocument/documentLink",
            json!({"textDocument":{"uri":new_uri}}),
        );
        assert_eq!(links["result"][0]["target"], uri(&root.join("dé😀.bend")));
        assert_eq!(
            links["result"][1]["target"],
            uri(&root.join("nested/src/sibling.bend"))
        );
        let next = "import ../../dé😀.bend as Dep # keep\nimport sibling.bend as Sibling # unchanged spelling\ndef main = Dep.value\ndef unsaved = Dep.value\n";
        change(&mut client, &new_uri, 14, next);
        let renamed = rename(
            &mut client,
            &new_uri,
            next,
            next.find("Dep #").must_be("alias"),
            "Module",
        );
        let edit = &renamed["result"]["documentChanges"][0];
        assert_eq!(edit["textDocument"]["version"], 14);
        assert_eq!(
            apply(next, &edit["edits"]),
            "import ../../dé😀.bend as Module # keep\nimport sibling.bend as Sibling # unchanged spelling\ndef main = Module.value\ndef unsaved = Module.value\n"
        );
        assert_eq!(
            fs::read_to_string(new_path).must_be("untouched disk text"),
            "def disk_only = 0\n"
        );
        client.finish();
    }

    #[test]
    fn rename_batch_chains_use_original_identities_and_reject_unsaved_collisions() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        let a = root.join("a.bend");
        let b = root.join("b.bend");
        let c = root.join("c.bend");
        fs::write(&a, "def from_a = 1\n").must_be("first module");
        fs::write(&b, "def from_b = 2\n").must_be("second module");
        let main = uri(&root.join("main.bend"));
        let source = "import ./a.bend as A\nimport ./b.bend as B\ndef main = A.from_a\n";
        let mut client = client(root);
        open(&mut client, &main, 1, source);
        let files = json!({"files":[{"oldUri":uri(&a),"newUri":uri(&b)},{"oldUri":uri(&b),"newUri":uri(&c)}]});
        let response = client.request("workspace/willRenameFiles", files.clone());
        let updated = apply(source, &response["result"]["documentChanges"][0]["edits"]);
        assert_eq!(
            updated,
            "import ./b.bend as A\nimport ./c.bend as B\ndef main = A.from_a\n"
        );
        fs::rename(&b, &c).must_be("client moves second original");
        fs::rename(&a, &b).must_be("client moves first original");
        change(&mut client, &main, 2, &updated);
        client.notify("workspace/didRenameFiles", files);
        let definition = client.request(
            "textDocument/definition",
            json!({"textDocument":{"uri":main},"position":{"line":2,"character":16}}),
        );
        assert_eq!(definition["result"]["uri"], uri(&b));
        open(&mut client, &uri(&b), 5, "def unsaved_first = 9\n");
        open(&mut client, &uri(&c), 8, "def unsaved_second = 10\n");
        let collision = client.request_response(
            "workspace/willRenameFiles",
            json!({"files":[{"oldUri":uri(&b),"newUri":uri(&c)}]}),
        );
        assert_eq!(collision["error"]["code"], -32602, "{collision}");
        let symbols = client.request("workspace/symbol", json!({"query":"unsaved_"}));
        let symbols = symbols["result"]
            .as_array()
            .must_be("preserved unsaved declarations");
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol["name"] == "unsaved_first")
        );
        assert!(
            symbols
                .iter()
                .any(|symbol| symbol["name"] == "unsaved_second")
        );
        client.finish();
    }

    #[test]
    fn file_rename_orders_immediate_change_close_and_reopen_at_new_uri() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        let old = root.join("source.bend");
        let new = root.join("renamed.bend");
        let consumer = root.join("consumer.bend");
        fs::write(&old, "def original = 1\n").must_be("source file");
        let mut closed = String::from("import ./source.bend as Source\n");
        for index in 0..4096 {
            writeln!(&mut closed, "def closed_{index} = Source.original").must_be("closed source");
        }
        fs::write(&consumer, &closed).must_be("closed consumer");
        let main = uri(&root.join("main.bend"));
        let mut client = client(root);
        open(
            &mut client,
            &main,
            1,
            "import ./consumer.bend as Consumer\ndef main = Consumer.closed_0\n",
        );
        open(&mut client, &uri(&old), 4, "def original = 1\n");
        let files = json!({"files":[{"oldUri":uri(&old),"newUri":uri(&new)}]});
        let proposed = client.request("workspace/willRenameFiles", files.clone());
        let edits = proposed["result"]["documentChanges"]
            .as_array()
            .must_be("rename changes");
        let consumer_edits = edits
            .iter()
            .find(|edit| edit["textDocument"]["uri"] == uri(&consumer))
            .must_be("closed consumer import edit");
        fs::write(&consumer, apply(&closed, &consumer_edits["edits"]))
            .must_be("client applies imports");
        fs::rename(&old, &new).must_be("client moves source");
        client.notify("workspace/didRenameFiles", files);
        let new_uri = uri(&new);
        change(&mut client, &new_uri, 5, "def during_move = 2\n");
        client.notify(
            "textDocument/didClose",
            json!({"textDocument":{"uri":new_uri}}),
        );
        open(&mut client, &new_uri, 1, "def reopened_latest = 3\n");
        let symbols = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":new_uri}}),
        );
        let names: Vec<_> = symbols["result"]
            .as_array()
            .must_be("reopened declarations")
            .iter()
            .map(|symbol| symbol["name"].as_str().must_be("declaration name"))
            .collect();
        assert_eq!(
            names,
            ["reopened_latest"],
            "rename must not drop new-URI edits or reverse close/reopen"
        );
        let definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":new_uri},"position":{"line":0,"character":6}
            }),
        );
        assert_eq!(definition["result"]["uri"], new_uri);
        client.finish();
    }

    #[test]
    fn file_rename_rejects_compiler_owned_base_without_losing_navigation() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        let main = uri(&root.join("main.bend"));
        let mut client = client(root);
        open(&mut client, &main, 1, "import Base\ndef main = builtin()\n");
        let links = client.request(
            "textDocument/documentLink",
            json!({"textDocument":{"uri":main}}),
        );
        let base_uri = links["result"][0]["target"]
            .as_str()
            .must_be("generated Base link");
        let base = Url::parse(base_uri)
            .must_be("Base URI")
            .to_file_path()
            .must_be("Base file");
        let destination = base.with_file_name("Renamed.bend");
        let rejected = client.request_response(
            "workspace/willRenameFiles",
            json!({
                "files":[{"oldUri":base_uri,"newUri":uri(&destination)}]
            }),
        );
        assert_eq!(rejected["error"]["code"], -32602);
        let definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":main},"position":{"line":1,"character":12}
            }),
        );
        assert_eq!(definition["result"]["uri"], base_uri);
        assert!(
            base.is_file(),
            "compiler navigation backing file remains owned and readable"
        );
        client.finish();
    }

    #[test]
    fn will_rename_uses_current_closed_file_ranges_without_watch_notification() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        let dep = root.join("dep.bend");
        let consumer = root.join("consumer.bend");
        fs::write(&dep, "def value = 1\n").must_be("dependency");
        fs::write(
            &consumer,
            "import ./dep.bend as Dep\ndef exported = Dep.value\n",
        )
        .must_be("consumer");
        let main = uri(&root.join("main.bend"));
        let mut client = client(root);
        open(
            &mut client,
            &main,
            1,
            "import ./consumer.bend as Consumer\ndef main = Consumer.exported\n",
        );
        let loaded = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":main},"position":{"line":1,"character":21}
            }),
        );
        assert_eq!(loaded["result"]["uri"], uri(&consumer));
        let updated = "# external 😀 comment\nimport ./dep.bend as Dep\ndef exported = Dep.value\n";
        fs::write(&consumer, updated).must_be("external edit without watcher");
        let response = client.request(
            "workspace/willRenameFiles",
            json!({
                "files":[{"oldUri":uri(&dep),"newUri":uri(&root.join("renamed.bend"))}]
            }),
        );
        let changes = response["result"]["documentChanges"]
            .as_array()
            .must_be("rename changes");
        let edit = changes
            .iter()
            .find(|edit| edit["textDocument"]["uri"] == uri(&consumer))
            .must_be("closed consumer edits");
        assert_eq!(
            apply(updated, &edit["edits"]),
            "# external 😀 comment\nimport ./renamed.bend as Dep\ndef exported = Dep.value\n"
        );
        client.finish();
    }

    #[test]
    fn closing_moved_open_importer_restores_current_disk_imports() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        fs::create_dir_all(root.join("src")).must_be("source directory");
        fs::create_dir_all(root.join("nested/src")).must_be("destination directory");
        let dep = root.join("dep.bend");
        let old = root.join("src/module.bend");
        let new = root.join("nested/src/module.bend");
        let source = "import ../dep.bend as Dep\ndef exported = Dep.value\n";
        fs::write(&dep, "def value = 1\n").must_be("dependency");
        fs::write(&old, source).must_be("importer");
        let main = uri(&root.join("main.bend"));
        let main_source = "import ./src/module.bend as Module\nimport ./dep.bend as Direct\ndef main = Module.exported\n";
        let mut client = client(root);
        open(&mut client, &main, 1, main_source);
        let loaded = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":main},"position":{"line":2,"character":19}
            }),
        );
        assert_eq!(loaded["result"]["uri"], uri(&old));
        open(&mut client, &uri(&old), 2, source);
        let files = json!({"files":[{"oldUri":uri(&old),"newUri":uri(&new)}]});
        let response = client.request("workspace/willRenameFiles", files.clone());
        let changes = response["result"]["documentChanges"]
            .as_array()
            .must_be("rename edits");
        let moved = changes
            .iter()
            .find(|edit| edit["textDocument"]["uri"] == uri(&old))
            .must_be("moved importer edit");
        let moved_source = apply(source, &moved["edits"]);
        let main_edit = changes
            .iter()
            .find(|edit| edit["textDocument"]["uri"] == main)
            .must_be("root import edit");
        change(
            &mut client,
            &main,
            2,
            &apply(main_source, &main_edit["edits"]),
        );
        change(&mut client, &uri(&old), 3, &moved_source);
        fs::write(&old, &moved_source).must_be("client saves moved importer");
        fs::rename(&old, &new).must_be("client moves importer");
        client.notify("workspace/didRenameFiles", files);
        client.notify(
            "textDocument/didClose",
            json!({"textDocument":{"uri":uri(&new)}}),
        );
        let reference_source = "import ./nested/src/module.bend as Module\nimport ./dep.bend as Direct\ndef main = Direct.value\n";
        change(&mut client, &main, 3, reference_source);
        let references = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":main},"position":{"line":2,"character":19},
                "context":{"includeDeclaration":false}
            }),
        );
        assert!(
            references["result"]
                .as_array()
                .must_be("dependency references")
                .iter()
                .any(|location| location["uri"] == uri(&new)
                    && location["range"]["start"]["line"] == 1),
            "closed moved importer must retain its saved relative dependency: {references}"
        );
        client.finish();
    }

    #[test]
    fn physical_file_rename_updates_indexed_symlink_import_alias() {
        let temp = tempfile::tempdir().must_be("workspace");
        let root = temp.path();
        fs::create_dir(root.join("real")).must_be("physical directory");
        std::os::unix::fs::symlink("real", root.join("link")).must_be("directory alias");
        let dep = root.join("real/dep.bend");
        let new = root.join("real/renamed.bend");
        fs::write(&dep, "def value = 1\n").must_be("dependency");
        let main = uri(&root.join("main.bend"));
        let source = "import ./link/dep.bend as Dep\ndef main = Dep.value\n";
        let mut client = client(root);
        open(&mut client, &main, 1, source);
        let before = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":main},"position":{"line":1,"character":16}
            }),
        );
        assert_eq!(before["result"]["uri"], uri(&root.join("link/dep.bend")));
        let files = json!({"files":[{"oldUri":uri(&dep),"newUri":uri(&new)}]});
        let response = client.request("workspace/willRenameFiles", files.clone());
        let edits = &response["result"]["documentChanges"][0]["edits"];
        let updated = apply(source, edits);
        assert_eq!(
            updated,
            "import ./real/renamed.bend as Dep\ndef main = Dep.value\n"
        );
        change(&mut client, &main, 2, &updated);
        fs::rename(&dep, &new).must_be("client moves physical target");
        client.notify("workspace/didRenameFiles", files);
        let after = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":main},"position":{"line":1,"character":16}
            }),
        );
        assert_eq!(after["result"]["uri"], uri(&new));
        client.finish();
    }
}
