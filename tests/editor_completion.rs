#[cfg(unix)]
#[path = "support/lsp_client.rs"]
pub mod lsp_client;
#[cfg(unix)]
mod support;

#[cfg(unix)]
mod protocol {
    use super::{lsp_client::LspClient, support::Must};
    use bend2_lsp::analysis::{DocumentSnapshot, Revision};
    use serde_json::{Value, json};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Command, Stdio},
    };
    use tempfile::{TempDir, tempdir};
    use url::Url;

    struct Editor {
        client: LspClient,
        uri: Url,
        source: String,
        temp: TempDir,
        version: i32,
    }

    impl Editor {
        fn new(source: &str, dependency: Option<&str>) -> Self {
            Self::with_setup(source, dependency, |_| {})
        }

        fn with_setup(source: &str, dependency: Option<&str>, setup: impl FnOnce(&Path)) -> Self {
            let temp = tempdir().must_be("temporary workspace");
            let root = temp.path();
            setup(root);
            let executable = root.join("bend");
            fs::write(&executable, "#!/bin/sh\nexit 0\n").must_be("write compiler fixture");
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
                .must_be("compiler permissions");
            if let Some(dependency) = dependency {
                fs::write(root.join("tools.bend"), dependency).must_be("write dependency");
            }
            let path = root.join("main.bend");
            fs::write(&path, source).must_be("write editor source");
            let uri = Url::from_file_path(path).must_be("source URI");
            let paths = std::iter::once(root.to_path_buf())
                .chain(std::env::split_paths(
                    &std::env::var_os("PATH").unwrap_or_default(),
                ))
                .collect::<Vec<_>>();
            let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
            command
                .env("PATH", std::env::join_paths(paths).must_be("fixture PATH"))
                .env("BEND_LIB", root.join("library"))
                .stderr(Stdio::null());
            let mut client = LspClient::spawn(command);
            client.initialize(root);
            if let Some(dependency) = dependency {
                let dependency_uri =
                    Url::from_file_path(root.join("tools.bend")).must_be("dependency URI");
                client.notify(
                    "textDocument/didOpen",
                    json!({"textDocument":{"uri":dependency_uri,"languageId":"bend","version":1,"text":dependency}}),
                );
                client.request(
                    "textDocument/documentSymbol",
                    json!({"textDocument":{"uri":dependency_uri}}),
                );
            }
            client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":source}}),
            );
            Self {
                client,
                uri,
                source: source.to_owned(),
                temp,
                version: 1,
            }
        }
        fn complete(&mut self, marker: &str) -> Vec<Value> {
            let response = self.complete_response(marker, &json!({"triggerKind":1}));
            let result = &response["result"];
            result
                .as_array()
                .or_else(|| result["items"].as_array())
                .must_be("completion result")
                .clone()
        }

        fn complete_response(&mut self, marker: &str, context: &Value) -> Value {
            let offset = self.source.find(marker).must_be("completion marker") + marker.len();
            let snapshot = DocumentSnapshot::new(Revision(1), self.source.clone());
            let (line, character) = snapshot.line_index.position(&self.source, offset);
            self.client.request(
                "textDocument/completion",
                json!({"textDocument":{"uri":self.uri},"position":{"line":line,"character":character},"context":context}),
            )
        }

        fn index_unsaved(&mut self, name: &str, source: &str) -> Url {
            let path = self.temp.path().join(name);
            let uri = Url::from_file_path(&path).must_be("candidate URI");
            self.client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":source}}),
            );
            // Availability uses published indexes, not unrelated pending builds.
            self.client.request(
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":uri}}),
            );
            assert!(!path.exists(), "indexing must not save the candidate");
            uri
        }

        fn replace(&mut self, source: &str) {
            source.clone_into(&mut self.source);
            self.version += 1;
            self.client.notify("textDocument/didChange", json!({"textDocument":{"uri":self.uri,"version":self.version},"contentChanges":[{"text":source}]}));
        }

        fn finish(self) {
            self.client.finish();
        }
    }

    fn item<'a>(items: &'a [Value], label: &str) -> &'a Value {
        items
            .iter()
            .find(|item| {
                item["label"].as_str().is_some_and(|candidate| {
                    candidate.strip_prefix("./").unwrap_or(candidate)
                        == label.strip_prefix("./").unwrap_or(label)
                })
            })
            .must_be("expected completion")
    }

    fn apply(source: &str, item: &Value) -> String {
        let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
        let mut edits: Vec<_> = std::iter::once(&item["textEdit"])
            .chain(item["additionalTextEdits"].as_array().into_iter().flatten())
            .collect();
        let position = |value: &Value| {
            snapshot.line_index.offset(
                source,
                u32::try_from(value["line"].as_u64().must_be("edit line")).must_be("line fits"),
                u32::try_from(value["character"].as_u64().must_be("edit character"))
                    .must_be("character fits"),
            )
        };
        edits.sort_by_key(|edit| {
            (
                position(&edit["range"]["start"]),
                position(&edit["range"]["end"]),
            )
        });
        for pair in edits.windows(2) {
            assert!(
                position(&pair[0]["range"]["end"]) < position(&pair[1]["range"]["start"]),
                "completion edits must not overlap or share an insertion boundary"
            );
        }
        let mut result = source.to_owned();
        for edit in edits.into_iter().rev() {
            result.replace_range(
                position(&edit["range"]["start"])..position(&edit["range"]["end"]),
                edit["newText"].as_str().must_be("edit text"),
            );
        }
        result
    }

    #[test]
    fn fuzzy_ranking_and_lexical_shadowing_are_deterministic() {
        let mut editor = Editor::new(
            "def add:\n  1\ndef alpha_delta:\n  2\ndef main(ad, adder):\n  ad",
            None,
        );
        let first = editor.complete("\n  ad");
        assert_eq!(first, editor.complete("\n  ad"));
        assert_eq!(first[0]["label"], "ad");
        let index = |label: &str| {
            first
                .iter()
                .position(|item| item["label"] == label)
                .must_be("ranked candidate")
        };
        assert!(index("adder") < index("add"));
        assert!(index("add") < index("alpha_delta"));
        editor.replace("def add:\n  1\ndef main(add):\n  ad");
        let shadowed = editor.complete("\n  ad");
        assert_eq!(item(&shadowed, "add")["kind"], 6);
        assert_eq!(
            shadowed
                .iter()
                .filter(|entry| entry["label"] == "add")
                .count(),
            1
        );
        editor.finish();
    }

    #[test]
    fn utf16_middle_token_edit_preserves_expression_suffix() {
        let source = "def add(x):\n  x\ndef main:\n  \"😀\"; adzz(1)\n";
        let mut editor = Editor::new(source, None);
        let items = editor.complete("\"😀\"; ad");
        let add = item(&items, "add");
        assert_eq!(
            add["textEdit"]["range"],
            json!({"start":{"line":3,"character":8},"end":{"line":3,"character":12}})
        );
        assert_eq!(
            apply(source, add),
            "def add(x):\n  x\ndef main:\n  \"😀\"; add(1)\n"
        );
        editor.finish();
    }

    #[test]
    fn qualified_subsequence_edits_preserve_alias_and_suffix() {
        let source = "import tools.bend as Tools\ndef main:\n  Tools.apdzz(1)\n";
        let mut editor = Editor::new(source, Some("def append(x):\n  x\n"));
        let items = editor.complete("Tools.apd");
        assert_eq!(
            apply(source, item(&items, "append")),
            "import tools.bend as Tools\ndef main:\n  Tools.append(1)\n"
        );
        editor.replace("import tools.bend as Tools\ndef main:\n  Tls");
        assert_eq!(item(&editor.complete("\n  Tls"), "Tools")["kind"], 9);
        editor.replace("import tools.bend as Tools\ndef main(Tools):\n  Tools.ap");
        assert!(editor.complete("Tools.ap").is_empty());
        editor.finish();
    }

    #[test]
    fn explicit_types_filter_nested_patterns_but_unknown_types_keep_candidates() {
        let declarations = "type Shape is Data:\n  Circle{}\ntype Other is Data:\n  Square{}\ntype Wrapped is Data:\n  Box{value: Shape}\n";
        let source =
            format!("{declarations}def main(value: Wrapped):\n  match value:\n    case Box{{Ci");
        let mut editor = Editor::new(&source, None);
        let nested = editor.complete("case Box{Ci");
        assert_eq!(item(&nested, "Circle")["kind"], 4);
        assert!(
            !nested.iter().any(|item| item["label"] == "Square"
                || item["label"] == "Box"
                || item["kind"] == 14)
        );
        editor.replace(&format!(
            "{declarations}def main(value: Wrapped):\n  match value:\n    case Box{{\n      Ci"
        ));
        assert_eq!(item(&editor.complete("\n      Ci"), "Circle")["kind"], 4);
        editor.replace(&format!(
            "{declarations}def main(value: Shape):\n  match value:\n    case "
        ));
        let known = editor.complete("case ");
        assert!(known.iter().any(|item| item["label"] == "Circle"));
        assert!(!known.iter().any(|item| item["label"] == "Square"));
        editor.replace(&format!(
            "{declarations}def main(value: Mystery):\n  match value:\n    case "
        ));
        let unknown = editor.complete("case ");
        assert!(unknown.iter().any(|item| item["label"] == "Circle"));
        assert!(unknown.iter().any(|item| item["label"] == "Square"));
        editor.finish();
    }

    #[test]
    fn unsaved_eof_rebuilds_nested_context_and_expression_context() {
        let mut editor = Editor::new("def old:\n  0\n", None);
        editor.replace(
            "type Tree is Data:\n  Leaf{}\ndef newest(value):\n  match value:\n    case Wrapper(Le",
        );
        let pattern = editor.complete("Wrapper(Le");
        assert_eq!(item(&pattern, "Leaf")["kind"], 4);
        assert!(!pattern.iter().any(|item| item["label"] == "newest"));
        editor.replace("def newest:\n  1\ndef main:\n  nw");
        assert_eq!(item(&editor.complete("\n  nw"), "newest")["kind"], 3);
        editor.finish();
    }

    #[test]
    fn import_continuations_offer_base_and_indexed_unsaved_local_sources() {
        let mut editor = Editor::new("import B", None);
        assert_eq!(
            item(&editor.complete("import B"), "Base")["textEdit"]["newText"],
            "Base"
        );
        let target = editor.temp.path().join("local.bend");
        let uri = Url::from_file_path(&target).must_be("indexed target URI");
        editor.client.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":"def local:\n  1\n"}}));
        editor.replace("import lcl as Local\ndef main:\n  1\n");
        let items = editor.complete("import lcl");
        let local = item(&items, "local.bend");
        assert_eq!(
            apply(&editor.source, local),
            "import local.bend as Local\ndef main:\n  1\n"
        );
        assert!(
            !Path::new(&target).exists(),
            "unsaved source must stay unsaved"
        );
        editor.finish();
    }

    #[test]
    fn import_acceptance_replaces_middle_path_and_preserves_explicit_alias() {
        let source = "# header 😀\nimport toOLD.bend as UserChosen # keep me\r\n";
        let mut editor = Editor::new(source, Some("def value:\n  1\n"));
        let items = editor.complete("import to");
        assert_eq!(
            apply(source, item(&items, "tools.bend")),
            "# header 😀\nimport tools.bend as UserChosen # keep me\r\n"
        );
        editor.replace("import toOLD.bend as # keep me\n");
        let items = editor.complete("import to");
        assert_eq!(
            apply(&editor.source, item(&items, "tools.bend")),
            "import tools.bend as Tools # keep me\n"
        );
        editor.replace("import BOLD as Wrong # keep me\n");
        let items = editor.complete("import B");
        assert_eq!(
            apply(&editor.source, item(&items, "Base")),
            "import Base # keep me\n"
        );
        editor.finish();
    }

    #[test]
    fn import_acceptance_generates_ascii_aliases_for_unicode_and_relative_paths() {
        let mut editor = Editor::new("import ", None);
        for (path, alias) in [
            ("./nested/tools.bend", "Tools"),
            ("../sibling.bend", "Sibling"),
            ("./nested/工具.bend", "Module"),
            ("./nested/😀value.bend", "Value"),
            ("./nested/123-name.bend", "Module123name"),
            ("./nested/_private.bend", "_private"),
        ] {
            let uri = editor.uri.join(path).must_be("indexed relative target URI");
            editor.client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":uri,"languageId":"bend","version":1,"text":"def value:\n  1\n"
                }}),
            );
            editor.client.request(
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":uri}}),
            );
            editor.replace("import ");
            let label = if path.starts_with("../") {
                path
            } else {
                &path[2..]
            };
            let items = editor.complete("import ");
            let edited = apply(&editor.source, item(&items, label));
            let snapshot = DocumentSnapshot::new(Revision(1), edited.clone());
            let import = snapshot.syntax.imports().first().must_be("accepted import");
            assert_eq!(
                editor
                    .uri
                    .join(import.path_text(&edited))
                    .must_be("accepted target URI"),
                uri
            );
            assert_eq!(import.alias_text(&edited), Some(alias));
            let alias = edited.rsplit_once(" as ").must_be("generated alias").1;
            assert!(alias.as_bytes()[0].is_ascii_alphabetic() || alias.starts_with('_'));
            assert!(
                alias
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            );
        }
        editor.finish();
    }

    #[test]
    fn import_filename_ranking_and_client_filtering_agree_for_automatic_requests() {
        let mut editor = Editor::new("import ad", None);
        for path in [
            "nested/ad.bend",
            "nested/adder.bend",
            "distant/alpha_delta.bend",
        ] {
            let uri = Url::from_file_path(editor.temp.path().join(path)).must_be("target URI");
            editor.client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":"def exported:\n  1\n"}}),
            );
            editor.client.request(
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":uri}}),
            );
        }
        // Ordinary Zed input uses INVOKED; clients may omit manual context.
        let automatic = editor.complete_response("import ad", &json!({"triggerKind":1}));
        let manual = editor.complete_response("import ad", &Value::Null);
        assert_eq!(automatic["result"], manual["result"]);
        let result = &automatic["result"];
        assert_eq!(
            result["isIncomplete"], true,
            "query-specific metadata must refresh"
        );
        let items = result["items"].as_array().must_be("import items");
        let exact = item(items, "./nested/ad.bend");
        let prefix = item(items, "./nested/adder.bend");
        let fuzzy = item(items, "./distant/alpha_delta.bend");
        assert_eq!(exact["filterText"], "ad");
        assert_eq!(prefix["filterText"], "adder");
        assert_eq!(fuzzy["filterText"], "alpha_delta");
        assert!(exact["sortText"].as_str() < prefix["sortText"].as_str());
        assert!(prefix["sortText"].as_str() < fuzzy["sortText"].as_str());
        for candidate in [exact, prefix, fuzzy] {
            assert!(
                bend2_lsp::analysis::completion_match(
                    candidate["filterText"].as_str().must_be("search text"),
                    "ad",
                )
                .is_some(),
                "a returned candidate must survive client filtering: {candidate}"
            );
            assert_eq!(
                candidate["textEdit"]["range"],
                json!({"start":{"line":0,"character":7},"end":{"line":0,"character":9}})
            );
        }
        editor.finish();
    }

    #[test]
    fn import_acceptance_avoids_declarations_bindings_aliases_and_unicode_fallback_collisions() {
        let source = "import other.bend as Tools\nimport toOLD.bend\ndef Tools2:\n  1\ndef Tools3.member:\n  2\ndef main(Tools4):\n  Tools5\n";
        let mut editor = Editor::new(source, Some("def value:\n  1\n"));
        let items = editor.complete("\nimport to");
        let edited = apply(source, item(&items, "tools.bend"));
        assert!(
            edited.contains("\nimport tools.bend as Tools6\n"),
            "{edited}"
        );
        let unicode =
            Url::from_file_path(editor.temp.path().join("工具.bend")).must_be("Unicode URI");
        editor.client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":unicode,"languageId":"bend","version":1,"text":"def value:\n  1\n"
            }}),
        );
        editor.client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":unicode}}),
        );
        editor.replace("import \ndef Module:\n  1\ndef Module2:\n  2\n");
        let items = editor.complete("import ");
        assert_eq!(
            apply(&editor.source, item(&items, "工具.bend")),
            "import 工具.bend as Module3\ndef Module:\n  1\ndef Module2:\n  2\n"
        );
        editor.finish();
    }

    #[test]
    fn import_popup_refreshes_after_path_separators_backspace_and_retyping() {
        let mut editor = Editor::new("import a", None);
        for path in [
            "nested/append.bend",
            "elsewhere/append.bend",
            "nested/unrelated.bend",
        ] {
            let uri = Url::from_file_path(editor.temp.path().join(path)).must_be("target URI");
            editor.client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":"def exported:\n  1\n"}}),
            );
            editor.client.request(
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":uri}}),
            );
        }
        for (prefix, search) in [
            ("a", "append"),
            ("apd", "append"),
            ("nst", "nested/append"),
            ("nested/", "append"),
            ("nested/ap", "append"),
            ("nested/apd", "append"),
            ("./nested/apd", "append"),
            ("nested/append.be", "append.bend"),
            ("nested/ap", "append"),
            ("apd", "append"),
        ] {
            editor.replace(&format!("import {prefix}"));
            let context = if prefix.ends_with('/') {
                json!({"triggerKind":2,"triggerCharacter":"/"})
            } else {
                json!({"triggerKind":1})
            };
            let response = editor.complete_response(&format!("import {prefix}"), &context);
            let result = &response["result"];
            assert_eq!(result["isIncomplete"], true);
            let items = result["items"].as_array().must_be("import items");
            assert_eq!(item(items, "./nested/append.bend")["filterText"], search);
            if prefix.contains("nested/") || prefix == "nst" {
                assert!(
                    !items
                        .iter()
                        .any(|candidate| candidate["label"] == "./elsewhere/append.bend")
                );
            }
            if prefix.ends_with("apd") {
                assert!(
                    !items
                        .iter()
                        .any(|candidate| candidate["label"] == "./nested/unrelated.bend")
                );
            }
        }
        editor.finish();
    }

    #[test]
    fn import_acceptance_reuses_existing_namespace_for_relative_spelling() {
        let source = "import ./tools.bend as Existing\nimport toOLD.bend # repeated\n";
        let mut editor = Editor::new(source, Some("def value:\n  1\n"));
        let items = editor.complete("\nimport to");
        assert_eq!(
            apply(source, item(&items, "tools.bend")),
            "import ./tools.bend as Existing\nimport tools.bend as Existing # repeated\n"
        );
        editor.finish();
    }

    #[test]
    fn import_separator_trigger_does_not_offer_paths_in_comments_strings_or_expressions() {
        let mut editor = Editor::new("# import nested/", None);
        let uri = Url::from_file_path(editor.temp.path().join("nested/append.bend"))
            .must_be("target URI");
        editor.client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":"def exported:\n  1\n"}}),
        );
        editor.client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        for source in [
            "# import nested/",
            "def main:\n  \"nested/\"",
            "def main:\n  1 /",
        ] {
            editor.replace(source);
            let marker = if source.contains('\"') {
                "\"nested/"
            } else {
                source
            };
            let response =
                editor.complete_response(marker, &json!({"triggerKind":2,"triggerCharacter":"/"}));
            let result = &response["result"];
            let items = result
                .as_array()
                .or_else(|| result["items"].as_array())
                .must_be("completion items");
            assert!(
                !items
                    .iter()
                    .any(|candidate| candidate["label"] == "./nested/append.bend")
            );
            if !source.ends_with("1 /") {
                assert!(items.is_empty(), "comments and strings suppress completion");
            }
        }
        editor.finish();
    }

    #[test]
    fn cached_package_import_continuation_uses_indexed_spelling() {
        let package = "0x0123456789abcdef0123456789abcdef";
        let source = format!("import {package}/library.bend as Cached\nimport {package}/li");
        let mut editor = Editor::with_setup(&source, None, |root| {
            let directory = root.join("library").join(package);
            fs::create_dir_all(&directory).must_be("package cache");
            fs::write(directory.join("library.bend"), "def cached:\n  1\n")
                .must_be("cached source");
        });
        let label = format!("{package}/library.bend");
        let items = editor.complete(&format!("\nimport {package}/li"));
        assert_eq!(
            apply(&source, item(&items, &label)),
            format!(
                "import {package}/library.bend as Cached\nimport {package}/library.bend as Cached"
            )
        );
        editor.finish();
    }

    fn imported_item<'a>(items: &'a [Value], label: &str, path: &str) -> &'a Value {
        items
            .iter()
            .find(|item| item["label"] == label && item["labelDetails"]["description"] == path)
            .must_be("source-distinguished completion")
    }

    #[test]
    fn indexed_candidates_distinguish_modules_rank_scope_first_and_avoid_alias_collisions() {
        let source = "def value_local:\n  0\ndef main(Tools):\n  value\n";
        let mut editor = Editor::new(source, None);
        let tools = editor.index_unsaved("tools.bend", "def value:\n  1\n");
        editor.index_unsaved("other.bend", "def value:\n  2\n");
        let offered = editor.complete("\n  value");
        assert_eq!(offered[0]["label"], "value_local");
        let tools_item = imported_item(&offered, "value", "./tools.bend");
        let other_item = imported_item(&offered, "value", "./other.bend");
        assert_eq!(tools_item["textEdit"]["newText"], "Tools2.value");
        assert_eq!(other_item["textEdit"]["newText"], "Other.value");
        assert!(
            tools_item["detail"]
                .as_str()
                .must_be("candidate detail")
                .contains("./tools.bend")
        );
        let accepted = apply(source, tools_item);
        assert_eq!(
            accepted,
            "import ./tools.bend as Tools2\ndef value_local:\n  0\ndef main(Tools):\n  Tools2.value\n"
        );
        editor.replace(&accepted);
        let offset = accepted.rfind("value").must_be("accepted member");
        let index = DocumentSnapshot::new(Revision(1), accepted.clone());
        let (line, character) = index.line_index.position(&accepted, offset);
        let definition = editor.client.request(
            "textDocument/definition",
            json!({"textDocument":{"uri":editor.uri},"position":{"line":line,"character":character}}),
        );
        assert_eq!(definition["result"]["uri"], json!(tools));
        editor.finish();
    }

    #[test]
    fn existing_alias_and_repeat_acceptance_use_latest_unsaved_imports_without_duplicates() {
        let source = "import tools.bend as Kit\ndef main:\n  val\n";
        let mut editor = Editor::new(source, Some("def value:\n  1\n"));
        let offered = editor.complete("\n  val");
        let value = imported_item(&offered, "value", "./tools.bend");
        assert!(value["additionalTextEdits"].is_null());
        assert_eq!(
            apply(source, value),
            "import tools.bend as Kit\ndef main:\n  Kit.value\n"
        );
        // The alias change exists only in the editor, not in the saved file.
        editor.replace("import tools.bend as Unsaved\ndef main:\n  val\n");
        let offered = editor.complete("\n  val");
        let first = apply(
            &editor.source,
            imported_item(&offered, "value", "./tools.bend"),
        );
        assert_eq!(
            first,
            "import tools.bend as Unsaved\ndef main:\n  Unsaved.value\n"
        );
        editor.replace(&format!("{first}  val\n"));
        let offered = editor.complete("\n  val");
        let second = apply(
            &editor.source,
            imported_item(&offered, "value", "./tools.bend"),
        );
        assert_eq!(
            second,
            "import tools.bend as Unsaved\ndef main:\n  Unsaved.value\n  Unsaved.value\n"
        );
        assert_eq!(
            fs::read_to_string(editor.temp.path().join("main.bend")).must_be("saved source"),
            source
        );
        editor.finish();
    }

    #[test]
    fn browsing_is_read_only_and_completion_matches_missing_import_quickfix() {
        let disk = "def main:\n  old\n";
        let source = "def main:\n  \"😀\"; value(1)\n";
        let mut editor = Editor::new(disk, None);
        editor.index_unsaved("tools.bend", "def value(x):\n  x\n");
        editor.replace(source);
        let offered = editor.complete("\"😀\"; value");
        assert_eq!(offered, editor.complete("\"😀\"; value"));
        assert_eq!(editor.source, source);
        assert_eq!(
            fs::read_to_string(editor.temp.path().join("main.bend")).must_be("saved source"),
            disk
        );
        let value = imported_item(&offered, "value", "./tools.bend");
        let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
        let offset = source.find("value").must_be("unresolved symbol");
        let (line, character) = snapshot.line_index.position(source, offset);
        let position = json!({"line":line,"character":character});
        let actions = editor.client.request(
            "textDocument/codeAction",
            json!({"textDocument":{"uri":editor.uri},
                "range":{"start":position,"end":position},
                "context":{"diagnostics":[],"only":["quickfix"]}}),
        );
        let action = actions["result"]
            .as_array()
            .must_be("quickfix actions")
            .iter()
            .find(|action| action["title"] == "Import value from ./tools.bend")
            .must_be("missing import remains offered");
        let quickfix_edits = action["edit"]["documentChanges"][0]["edits"]
            .as_array()
            .must_be("quickfix edits");
        let synthetic_completion = json!({
            "textEdit":quickfix_edits[1],
            "additionalTextEdits":[quickfix_edits[0]]
        });
        assert_eq!(apply(source, value), apply(source, &synthetic_completion));
        assert_eq!(
            apply(source, value),
            "import ./tools.bend as Tools\ndef main:\n  \"😀\"; Tools.value(1)\n"
        );
        editor.finish();
    }

    #[test]
    fn auto_import_at_replacement_boundary_is_one_nonoverlapping_primary_edit() {
        let mut editor = Editor::new("valzz", None);
        editor.index_unsaved("tools.bend", "def value:\n  1\n");
        let offered = editor.complete("val");
        let value = imported_item(&offered, "value", "./tools.bend");
        assert_eq!(
            apply(&editor.source, value),
            "import ./tools.bend as Tools\nTools.value"
        );
        assert_eq!(value["additionalTextEdits"], json!([]));
        editor.finish();
    }
    #[test]
    fn auto_import_completion_waits_for_a_new_unopened_watched_source() {
        let source = "def main:\n  project_can\n";
        let mut editor = Editor::new(source, None);
        editor
            .client
            .request("workspace/symbol", json!({"query":"main"}));
        let path = editor.temp.path().join("library.bend");
        let mut library = "def project_candidate:\n  1\n".to_owned();
        library.push_str(&"# unrelated source lines\n".repeat(40_000));
        fs::write(&path, library).must_be("write unopened library");
        let uri = Url::from_file_path(&path).must_be("library URI");
        editor.client.notify(
            "workspace/didChangeWatchedFiles",
            json!({
                "changes":[{"uri":uri,"type":1}]
            }),
        );
        let offered = editor.complete("\n  project_can");
        let candidate = imported_item(&offered, "project_candidate", "./library.bend");
        assert_eq!(
            apply(source, candidate),
            "import ./library.bend as Library\ndef main:\n  Library.project_candidate\n"
        );
        assert_eq!(
            fs::read_to_string(editor.temp.path().join("main.bend")).must_be("disk source"),
            source
        );
        editor.finish();
    }

    #[test]
    fn unsaved_candidate_revisions_and_middle_token_suffix_respect_discovery_exclusions() {
        let mut editor = Editor::with_setup("def main:\n  old\n", None, |root| {
            fs::write(root.join(".gitignore"), "hidden.bend\n").must_be("exclude disk candidate");
            fs::write(root.join("hidden.bend"), "def newest:\n  9\n").must_be("unindexed source");
        });
        let uri = editor.index_unsaved("tools.bend", "def stale:\n  0\n");
        editor.client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":2},
                "contentChanges":[{"text":"def newest:\n  1\n"}]}),
        );
        editor.client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        editor.replace("def main:\n  \"😀\"; nwzz(1)\n");
        let offered = editor.complete("\"😀\"; nw");
        let newest = imported_item(&offered, "newest", "./tools.bend");
        assert_eq!(
            apply(&editor.source, newest),
            "import ./tools.bend as Tools\ndef main:\n  \"😀\"; Tools.newest(1)\n"
        );
        assert!(
            !offered
                .iter()
                .any(|item| { item["labelDetails"]["description"] == "./hidden.bend" })
        );
        editor.replace("def main:\n  stale\n");
        assert!(!editor.complete("\n  stale").iter().any(|item| {
            item["label"] == "stale" && item["labelDetails"]["description"] == "./tools.bend"
        }));
        editor.finish();
    }

    #[test]
    fn indexed_external_constructors_keep_unknown_pattern_candidates_constructor_only() {
        let mut editor = Editor::new(
            "def main(value: Mystery):\n  match value:\n    case Ci",
            None,
        );
        editor.index_unsaved(
            "shapes.bend",
            "type Shape is Data:\n  Circle{}\ndef CircleFactory:\n  1\n",
        );
        let offered = editor.complete("case Ci");
        let circle = imported_item(&offered, "Circle", "./shapes.bend");
        assert_eq!(circle["kind"], 4);
        assert_eq!(
            apply(&editor.source, circle),
            "import ./shapes.bend as Shapes\ndef main(value: Mystery):\n  match value:\n    case Shapes.Circle"
        );
        assert!(!offered.iter().any(|item| item["label"] == "CircleFactory"));
        editor.finish();
    }

    #[test]
    fn already_loaded_base_candidates_add_bare_base_import_only_on_acceptance() {
        let mut editor = Editor::new("def main:\n  builtin\n", None);
        let compiler = editor.temp.path().join("compiler-fixture");
        fs::write(&compiler, "#!/bin/sh\ncase \"$1\" in\nversion) printf 'Bend test\\n';;\n--help) printf 'Bend\\nusage: bend <file> --check-only\\nbend base\\n';;\nbase) printf 'def builtin: U32\\n  1\\n';;\nesac\nexit 0\n")
            .must_be("compiler fixture");
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .must_be("compiler fixture permissions");
        editor.client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{"compilerPath":compiler}}}),
        );
        editor.replace("import Base\ndef main:\n  builtin\n");
        assert_eq!(item(&editor.complete("\n  builtin"), "builtin")["kind"], 3);
        editor.replace("def main:\n  builtin\n");
        let offered = editor.complete("\n  builtin");
        let builtin = imported_item(&offered, "builtin", "Base");
        let accepted = apply(&editor.source, builtin);
        assert_eq!(accepted, "import Base\ndef main:\n  builtin\n");
        editor.replace(&accepted);
        let offered = editor.complete("\n  builtin");
        assert!(item(&offered, "builtin")["additionalTextEdits"].is_null());
        assert!(!offered.iter().any(|item| {
            item["label"] == "builtin" && item["labelDetails"]["description"] == "Base"
        }));
        editor.replace("def main(builtin):\n  builtin\n");
        let offered = editor.complete("\n  builtin");
        assert_eq!(item(&offered, "builtin")["kind"], 6);
        assert!(!offered.iter().any(|item| {
            item["label"] == "builtin" && item["labelDetails"]["description"] == "Base"
        }));
        editor.finish();
    }

    #[test]
    fn unavailable_existing_alias_does_not_create_duplicate_or_namespace_changing_imports() {
        let mut editor = Editor::new(
            "import tools.bend as Kit\ndef main(Kit):\n  value\n",
            Some("def value:\n  1\n"),
        );
        assert!(!editor.complete("\n  value").iter().any(|item| {
            item["label"] == "value" && item["labelDetails"]["description"] == "./tools.bend"
        }));
        editor.replace("import tools.bend\ndef main:\n  value\n");
        assert!(!editor.complete("\n  value").iter().any(|item| {
            item["label"] == "value" && item["labelDetails"]["description"] == "./tools.bend"
        }));
        editor.finish();
    }
}
