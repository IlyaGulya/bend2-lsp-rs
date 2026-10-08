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
            let offset = self.source.find(marker).must_be("completion marker") + marker.len();
            let snapshot = DocumentSnapshot::new(Revision(1), self.source.clone());
            let (line, character) = snapshot.line_index.position(&self.source, offset);
            self.client.request("textDocument/completion", json!({"textDocument":{"uri":self.uri},"position":{"line":line,"character":character}}))["result"]
                .as_array().must_be("completion result").clone()
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
            .find(|item| item["label"] == label)
            .must_be("expected completion")
    }

    fn apply(source: &str, item: &Value) -> String {
        let snapshot = DocumentSnapshot::new(Revision(1), source.to_owned());
        let edit = &item["textEdit"];
        let position = |value: &Value| {
            snapshot.line_index.offset(
                source,
                u32::try_from(value["line"].as_u64().must_be("edit line")).must_be("line fits"),
                u32::try_from(value["character"].as_u64().must_be("edit character"))
                    .must_be("character fits"),
            )
        };
        let start = position(&edit["range"]["start"]);
        let end = position(&edit["range"]["end"]);
        format!(
            "{}{}{}",
            &source[..start],
            edit["newText"].as_str().must_be("edit text"),
            &source[end..]
        )
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
            let uri = Url::from_file_path(editor.temp.path().join(path))
                .must_be("indexed relative target URI");
            editor.client.notify("textDocument/didOpen", json!({"textDocument":{
                "uri":uri,"languageId":"bend","version":1,"text":"def value:\n  1\n"
            }}));
            editor.client.request("textDocument/documentSymbol", json!({"textDocument":{"uri":uri}}));
            editor.replace("import ");
            let label = if path.starts_with("../") { path } else { &path[2..] };
            let items = editor.complete("import ");
            let edited = apply(&editor.source, item(&items, label));
            assert_eq!(edited, format!("import {label} as {alias}"));
            let alias = edited.rsplit_once(" as ").must_be("generated alias").1;
            assert!(alias.as_bytes()[0].is_ascii_alphabetic() || alias.starts_with('_'));
            assert!(alias.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'));
        }
        editor.finish();
    }

    #[test]
    fn import_acceptance_avoids_declarations_bindings_aliases_and_unicode_fallback_collisions() {
        let source = "import other.bend as Tools\nimport toOLD.bend\ndef Tools2:\n  1\ndef Tools3.member:\n  2\ndef main(Tools4):\n  Tools5\n";
        let mut editor = Editor::new(source, Some("def value:\n  1\n"));
        let items = editor.complete("\nimport to");
        let edited = apply(source, item(&items, "tools.bend"));
        assert!(edited.contains("\nimport tools.bend as Tools6\n"), "{edited}");
        let unicode = Url::from_file_path(editor.temp.path().join("工具.bend")).must_be("Unicode URI");
        editor.client.notify("textDocument/didOpen", json!({"textDocument":{
            "uri":unicode,"languageId":"bend","version":1,"text":"def value:\n  1\n"
        }}));
        editor.client.request("textDocument/documentSymbol", json!({"textDocument":{"uri":unicode}}));
        editor.replace("import \ndef Module:\n  1\ndef Module2:\n  2\n");
        let items = editor.complete("import ");
        assert_eq!(
            apply(&editor.source, item(&items, "工具.bend")),
            "import 工具.bend as Module3\ndef Module:\n  1\ndef Module2:\n  2\n"
        );
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
            format!("import {package}/library.bend as Cached\nimport {package}/library.bend as Cached")
        );
        editor.finish();
    }
}
