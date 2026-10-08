#[cfg(unix)]
#[path = "support/lsp_client.rs"]
pub mod lsp_client;
#[cfg(unix)]
mod support;

#[cfg(unix)]
mod compiler {
    use super::{lsp_client::LspClient, support::Must};
    use serde_json::{Value, json};
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::Path,
        process::{Command, Stdio},
        time::Duration,
    };
    use tempfile::{TempDir, tempdir};
    use url::Url;

    fn fixture(body: &str, help: &str, version: &str) -> (TempDir, LspClient) {
        let directory = tempdir().must_be("temporary compiler workspace");
        let executable = directory.path().join("bend");
        let script = format!(
            "#!/bin/sh\nfor arg in \"$@\"; do\ncase \"$arg\" in\nversion) printf 'version\\n' >> \"$0.probes\"; printf '%s\\n' '{version}'; exit 0;;\n--help) printf 'help\\n' >> \"$0.probes\"; printf '%s\\n' '{help}'; exit 0;;\nbase) exit 0;;\nesac\ndone\n{body}\n",
        );
        fs::write(&executable, script).must_be("write compiler replay fixture");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .must_be("make compiler replay executable");
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
        command.stderr(Stdio::null()).env("PATH", directory.path());
        let mut client = LspClient::spawn(command);
        client.initialize(directory.path());
        (directory, client)
    }

    fn compatible(body: &str) -> (TempDir, LspClient) {
        fixture(
            body,
            "usage: bend <file.bend> --check-only; bend base",
            "bend 2.0.99",
        )
    }

    fn uri(path: &Path) -> String {
        Url::from_file_path(path)
            .must_be("document URI")
            .to_string()
    }

    fn open(client: &mut LspClient, uri: &str, source: &str) {
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"bend","version":1,"text":source}}),
        );
    }

    fn diagnostics(client: &mut LspClient, uri: &str, version: Option<i32>) -> Vec<Value> {
        let message = client
            .receive_matching(Duration::from_secs(10), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && version.is_none_or(|version| message["params"]["version"] == version)
            })
            .must_be("receive compiler diagnostics");
        message["params"]["diagnostics"]
            .as_array()
            .must_be("diagnostic array")
            .clone()
    }

    #[test]
    fn real_error_context_is_one_diagnostic_with_exact_caret_range() {
        // Replay the observed Bend 2.0.34 error, not separate expected,
        // observed and context errors. That compiler stops at its first failure.
        let body = "cat >&2 <<'ERROR'\nSOME PROOFS FAIL\nError:\n- expected : a defined name\n- observed : missing_first\nContext:\n- value : U32\nLocation: first\n3 | def first(value: U32) -> U32:\n4>|   missing_first\n  |   ^^^^^^^^^^^^^\n5 | \nERROR\nexit 1";
        // /bin/cat is explicit because this isolated fixture's PATH has only Bend.
        let body = body.replacen("cat", "/bin/cat", 1);
        let (directory, mut client) = compatible(&body);
        let document = uri(&directory.path().join("main.bend"));
        open(
            &mut client,
            &document,
            "import Base\n\ndef first(value: U32) -> U32:\n  missing_first\n\ndef second(value: U32) -> U32:\n  missing_second\n",
        );
        let items = diagnostics(&mut client, &document, Some(1));
        assert_eq!(items.len(), 1, "one actual compiler failure is one error");
        assert_eq!(
            items[0]["range"],
            json!({"start":{"line":3,"character":2},"end":{"line":3,"character":15}})
        );
        assert_eq!(
            items[0]["message"],
            "- expected : a defined name\n- observed : missing_first\nContext:\n- value : U32"
        );
        client.finish();
    }

    #[test]
    fn all_real_blocks_on_both_streams_are_mapped_without_context_errors() {
        let body = "/bin/cat >&2 <<'ERROR'\nError:\n- message  : root failure\nLocation: main\n2>| def main: U32\n  |     ^^^^\n3 |   0\nERROR\n/bin/cat <<'ERROR'\nError:\n- message  : imported failure\nContext:\n- note : U32\nLocation: Dep.value\n1>| def value: U32\n  |     ^^^^^\n2 |   1\nERROR\nexit 1";
        let (directory, mut client) = compatible(body);
        let dependency = directory.path().join("dep.bend");
        fs::write(&dependency, "def value: U32\n  1\n").must_be("write imported source");
        let root = uri(&directory.path().join("main.bend"));
        open(
            &mut client,
            &root,
            "import dep.bend as Dep\ndef main: U32\n  0\n",
        );
        let root_items = diagnostics(&mut client, &root, Some(1));
        let imported_items = diagnostics(&mut client, &uri(&dependency), None);
        assert_eq!(root_items.len(), 1);
        assert_eq!(imported_items.len(), 1);
        assert_eq!(root_items[0]["message"], "- message  : root failure");
        assert_eq!(
            imported_items[0]["message"],
            "- message  : imported failure\nContext:\n- note : U32"
        );
        assert_eq!(
            imported_items[0]["range"],
            json!({"start":{"line":0,"character":4},"end":{"line":0,"character":9}})
        );
        client.finish();
    }

    #[test]
    fn neighboring_lines_disambiguate_identical_marked_import_lines() {
        let body = "printf 'Error:\nimported error\nLocation: Dep.value\n2>| def value: U32\n3 |   2\n' >&2\nexit 1";
        let (directory, mut client) = compatible(body);
        let dependency = directory.path().join("dep.bend");
        fs::write(&dependency, "# dependency\ndef value: U32\n  2\n").must_be("write import");
        let root = uri(&directory.path().join("main.bend"));
        open(
            &mut client,
            &root,
            "import dep.bend as Dep\ndef value: U32\n  1\n",
        );
        assert!(diagnostics(&mut client, &root, Some(1)).is_empty());
        let imported = diagnostics(&mut client, &uri(&dependency), None);
        assert_eq!(
            imported[0]["range"],
            json!({"start":{"line":1,"character":0},"end":{"line":1,"character":14}})
        );
        assert_eq!(imported[0]["message"], "imported error");
        client.finish();
    }

    #[test]
    fn ambiguous_excerpt_is_not_assigned_to_an_imported_file() {
        let body = "printf 'Error:\nambiguous error\nLocation:\n2>| def value: U32\n' >&2\nexit 1";
        let (directory, mut client) = compatible(body);
        fs::write(
            directory.path().join("dep.bend"),
            "# dependency\ndef value: U32\n  1\n",
        )
        .must_be("write import");
        let root = uri(&directory.path().join("main.bend"));
        open(
            &mut client,
            &root,
            "import dep.bend as Dep\ndef value: U32\n  1\n",
        );
        let items = diagnostics(&mut client, &root, Some(1));
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]["range"],
            json!({"start":{"line":0,"character":0},"end":{"line":0,"character":0}})
        );
        client.finish();
    }

    #[test]
    fn caret_coordinates_preserve_utf16_in_unsaved_source() {
        let body = "printf 'Error:\nundefined name\nLocation:\n2>|   \"😀\" + missing\n |          ^^^^^^^\n' >&2\nexit 1";
        let (directory, mut client) = compatible(body);
        let root = uri(&directory.path().join("main.bend"));
        fs::write(directory.path().join("main.bend"), "def main: U32\n  0\n")
            .must_be("write stale disk source");
        open(&mut client, &root, "def main: U32\n  \"😀\" + missing\n");
        let items = diagnostics(&mut client, &root, Some(1));
        assert_eq!(
            items[0]["range"],
            json!({"start":{"line":1,"character":9},"end":{"line":1,"character":16}})
        );
        client.finish();
    }

    #[test]
    fn newer_version_with_real_cli_contract_is_accepted_and_probes_cached() {
        let (directory, mut client) = compatible("exit 0");
        let root = uri(&directory.path().join("main.bend"));
        open(&mut client, &root, "def main: U32\n  0\n");
        assert!(diagnostics(&mut client, &root, Some(1)).is_empty());
        client.notify("textDocument/didChange", json!({"textDocument":{"uri":root,"version":2},"contentChanges":[{"text":"def main: U32\n  1\n"}]}));
        assert!(diagnostics(&mut client, &root, Some(2)).is_empty());
        let probes = fs::read_to_string(directory.path().join("bend.probes"))
            .must_be("read compatibility probes");
        assert_eq!(
            probes, "version\nhelp\n",
            "document changes must not spawn compatibility probes"
        );
        client.finish();
    }

    #[test]
    fn recognized_cli_without_check_only_is_actionably_incompatible() {
        let (directory, mut client) = fixture(
            "exit 0",
            "usage: bend <file.bend> [args]; bend base",
            "bend 2.0.16",
        );
        let root = uri(&directory.path().join("main.bend"));
        open(&mut client, &root, "def main: U32\n  0\n");
        let items = diagnostics(&mut client, &root, Some(1));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["code"], "compiler-incompatible");
        assert!(
            items[0]["message"]
                .as_str()
                .must_be("incompatibility explanation")
                .contains("--check-only")
        );
        assert!(
            items[0]["message"]
                .as_str()
                .must_be("configuration remedy")
                .contains("compilerPath")
        );
        client.finish();
    }

    #[test]
    fn unknown_probe_contract_does_not_hide_actual_compiler_errors() {
        let (directory, mut client) = fixture(
            "printf 'Error:\noriginal textual failure\nLocation:\n1>| def main: U32\n' >&2\nexit 1",
            "",
            "",
        );
        let root = uri(&directory.path().join("main.bend"));
        open(&mut client, &root, "def main: U32\n  0\n");
        let items = diagnostics(&mut client, &root, Some(1));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["message"], "original textual failure");
        assert_eq!(items[0]["code"], "checking");
        client.finish();
    }

    #[test]
    fn compiler_exception_without_error_block_is_not_silently_discarded() {
        let (directory, mut client) =
            compatible("printf 'RangeError: unexpected compiler failure\\n' >&2\nexit 1");
        let root = uri(&directory.path().join("main.bend"));
        open(&mut client, &root, "def main: U32\n  0\n");
        let items = diagnostics(&mut client, &root, Some(1));
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]["message"],
            "RangeError: unexpected compiler failure"
        );
        assert_eq!(
            items[0]["range"],
            json!({"start":{"line":0,"character":0},"end":{"line":0,"character":0}})
        );
        client.finish();
    }

    #[test]
    fn invalid_caret_boundaries_fall_back_to_the_validated_source_line() {
        let body = "printf 'Error:\nundefined name\nLocation:\n2>|   \"😀\" + missing\n |     ^\n' >&2\nexit 1";
        let (directory, mut client) = compatible(body);
        let root = uri(&directory.path().join("main.bend"));
        open(&mut client, &root, "def main: U32\n  \"😀\" + missing\n");
        let items = diagnostics(&mut client, &root, Some(1));
        // Column four is the middle of the emoji's UTF-16 surrogate pair.
        assert_eq!(
            items[0]["range"],
            json!({"start":{"line":1,"character":0},"end":{"line":1,"character":16}})
        );
        client.finish();
    }

    fn install_recovered_compiler(path: &Path) {
        fs::write(path, "#!/bin/sh\ncase \"$1\" in\nversion) printf 'bend 2.0.99\\n';;\n--help) printf 'usage: bend <file.bend> --check-only; bend base\\n';;\nesac\nexit 0\n")
            .must_be("install compatible compiler");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .must_be("compiler permissions");
    }

    #[test]
    fn installing_initially_missing_compiler_recovers_without_configuration_change() {
        let directory = tempdir().must_be("temporary compiler workspace");
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
        command.stderr(Stdio::null()).env("PATH", directory.path());
        let mut client = LspClient::spawn(command);
        client.initialize(directory.path());
        let root = uri(&directory.path().join("main.bend"));
        open(&mut client, &root, "def main: U32\n  0\n");
        let missing = diagnostics(&mut client, &root, Some(1));
        assert_eq!(missing[0]["code"], "compiler-unavailable");
        install_recovered_compiler(&directory.path().join("bend"));
        client.notify("textDocument/didChange", json!({
            "textDocument":{"uri":root,"version":2},"contentChanges":[{"text":"def main: U32\n  1\n"}]
        }));
        assert!(
            diagnostics(&mut client, &root, Some(2)).is_empty(),
            "installing the configured executable must resume actual checks"
        );
        client.finish();
    }

    #[test]
    fn replacing_incompatible_compiler_recovers_without_configuration_change() {
        let (directory, mut client) = fixture(
            "exit 0",
            "usage: bend <file.bend> [args]; bend base",
            "bend 2.0.16",
        );
        let root = uri(&directory.path().join("main.bend"));
        open(&mut client, &root, "def main: U32\n  0\n");
        assert_eq!(
            diagnostics(&mut client, &root, Some(1))[0]["code"],
            "compiler-incompatible"
        );
        install_recovered_compiler(&directory.path().join("bend"));
        client.notify("textDocument/didChange", json!({
            "textDocument":{"uri":root,"version":2},"contentChanges":[{"text":"def main: U32\n  1\n"}]
        }));
        assert!(
            diagnostics(&mut client, &root, Some(2)).is_empty(),
            "same-path replacement must invalidate an incompatible CLI result"
        );
        client.finish();
    }
}
