#[cfg(unix)]
#[path = "support/lsp_client.rs"]
mod lsp_client;
#[cfg(unix)]
mod support;

#[cfg(unix)]
mod protocol {
    use super::{lsp_client::LspClient, support::Must};
    use serde_json::{Value, json};
    use std::{
        collections::HashMap,
        ffi::OsString,
        fmt::Write as _,
        fs,
        io::{BufRead, BufReader, Write},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::mpsc::{self, Receiver},
        thread,
        time::{Duration, Instant},
    };
    use tempfile::tempdir;
    use url::Url;

    fn spawn_client(compiler_dir: &Path) -> LspClient {
        spawn_with_optional_bend_lib_and_metrics(compiler_dir, None, None)
    }

    fn spawn_with_bend_lib(compiler_dir: &Path, bend_lib: &Path) -> LspClient {
        spawn_with_optional_bend_lib_and_metrics(compiler_dir, Some(bend_lib), None)
    }

    fn spawn_with_compiler_metrics(compiler_dir: &Path, metrics_path: &Path) -> LspClient {
        spawn_with_optional_bend_lib_and_metrics(compiler_dir, None, Some(metrics_path))
    }

    fn spawn_with_optional_bend_lib_and_metrics(
        compiler_dir: &Path,
        bend_lib: Option<&Path>,
        metrics_path: Option<&Path>,
    ) -> LspClient {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
        command
            .stderr(Stdio::null())
            .env("PATH", path_with_prefix(compiler_dir));
        if let Some(metrics_path) = metrics_path {
            command.env("BEND2_LSP_COMPILER_METRICS_FILE", metrics_path);
        }
        if let Some(bend_lib) = bend_lib {
            command.env("BEND_LIB", bend_lib);
        }
        LspClient::spawn(command)
    }

    fn request_burst_results(
        client: &mut LspClient,
        requests: &[(&str, Value)],
    ) -> Vec<(Value, Duration)> {
        let mut pending = HashMap::with_capacity(requests.len());
        for (method, params) in requests {
            let id = client.send_request(method, params.clone());
            pending.insert(id, Instant::now());
        }
        let mut responses = Vec::with_capacity(pending.len());
        for _ in 0..pending.len() {
            let (message, received) = client
                .receive_matching_timed(Duration::from_secs(10), |message| {
                    message
                        .get("id")
                        .and_then(Value::as_i64)
                        .is_some_and(|id| pending.contains_key(&id))
                })
                .must_be("receive pipelined LSP response");
            assert!(
                message.get("error").is_none(),
                "LSP request failed: {message}"
            );
            let id = message["id"]
                .as_i64()
                .must_be("pipelined response ID must be an integer");
            let sent = pending
                .remove(&id)
                .must_be("pipelined request must have a send timestamp");
            responses.push((message, received.duration_since(sent)));
        }
        responses
    }

    fn diagnostics_for(client: &mut LspClient, uri: &str, empty: bool, timeout: Duration) -> bool {
        client
            .receive_matching(timeout, |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && message["params"]["diagnostics"]
                        .as_array()
                        .is_some_and(|items| items.is_empty() == empty)
            })
            .is_some()
    }

    fn semantic_token_number(value: &Value) -> u32 {
        u32::try_from(
            value
                .as_u64()
                .must_be("semantic token field must be an unsigned integer"),
        )
        .must_be("semantic token field must fit the LSP u32 field")
    }

    fn path_with_prefix(prefix: &Path) -> OsString {
        let old = std::env::var_os("PATH").unwrap_or_default();
        let paths = std::iter::once(prefix.to_path_buf()).chain(std::env::split_paths(&old));
        std::env::join_paths(paths).must_be("construct test PATH")
    }

    fn install_compiler_stub(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).must_be("create stub bin directory");
        let executable = dir.join("bend");
        fs::write(
            &executable,
            "#!/bin/sh\ndep=\"${1%/*}/dep.bend\"\nif grep -q 'dep\\.bend as Dep$' \"$1\" && grep -q '^BAD$' \"$dep\"; then\n  printf 'Error:\\nsynthetic imported error\\nLocation:\\n1>| BAD\\n' >&2\n  exit 1\nfi\nexit 0\n",
        ).must_be("write compiler stub");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .must_be("make compiler stub executable");
        dir.to_path_buf()
    }
    fn create_fifo(path: &Path) {
        assert!(
            Command::new("mkfifo")
                .arg(path)
                .status()
                .must_be("create compiler synchronization FIFO")
                .success(),
            "mkfifo failed for {}",
            path.display()
        );
    }

    fn read_fifo_line(path: PathBuf) -> Receiver<String> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut line = String::new();
            BufReader::new(fs::File::open(path).must_be("open compiler synchronization FIFO"))
                .read_line(&mut line)
                .must_be("read compiler synchronization FIFO");
            sender
                .send(line.trim_end().to_owned())
                .must_be("send compiler synchronization event");
        });
        receiver
    }

    fn write_fifo_line(path: &Path, line: &str) {
        let mut fifo = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .must_be("open compiler release FIFO");
        fifo.write_all(line.as_bytes())
            .must_be("write compiler release FIFO");
        fifo.flush().must_be("flush compiler release FIFO");
    }

    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
    }

    fn install_blocked_diagnostics_compiler(
        dir: &Path,
        started_fifo: &Path,
        blocked_fifo: &Path,
        release_fifo: &Path,
        completion_fifo: &Path,
    ) -> PathBuf {
        fs::create_dir_all(dir).must_be("create blocked compiler directory");
        let executable = dir.join("bend");
        let script = format!(
            "#!/bin/sh\nif grep -q '^# BLOCKED_V1$' \"$1\"; then\n  (\n    IFS= read -r _ < {release}\n    printf 'released\\n' > {released}\n  ) >/dev/null 2>&1 &\n  printf '%s\\n' \"$$\" > {started}\n  IFS= read -r _ < {blocked}\nfi\ndep=\"${{1%/*}}/dep.bend\"\nif grep -q '^BAD$' \"$1\"; then\n  printf 'Error:\\nstale root compiler error\\nLocation:\\n2>| BAD\\n' >&2\n  exit 1\nfi\nif grep -q 'dep[.]bend as Dep$' \"$1\" && grep -q '^BAD$' \"$dep\"; then\n  printf 'Error:\\nstale imported compiler error\\nLocation:\\n1>| BAD\\n' >&2\n  exit 1\nfi\nexit 0\n",
            release = shell_quote(release_fifo),
            released = shell_quote(completion_fifo),
            started = shell_quote(started_fifo),
            blocked = shell_quote(blocked_fifo),
        );
        fs::write(&executable, script).must_be("write blocked compiler stub");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .must_be("make blocked compiler executable");
        dir.to_path_buf()
    }

    fn compiler_run_count(path: &Path) -> usize {
        fs::read_to_string(path)
            .must_be("read compiler invocation count")
            .lines()
            .count()
    }
    fn wait_for_clean_diagnostics(client: &mut LspClient, uri: &str) {
        assert!(
            diagnostics_for(client, uri, true, Duration::from_secs(5)),
            "expected empty diagnostics for {uri}"
        );
    }

    fn open_clean_document(client: &mut LspClient, uri: &str, source: &str) {
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        wait_for_clean_diagnostics(client, uri);
    }

    fn change_clean_document(client: &mut LspClient, uri: &str, version: i32, source: &str) {
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":uri,"version":version},
                "contentChanges":[{"text":source}]
            }),
        );
        wait_for_clean_diagnostics(client, uri);
    }

    fn notify_watched_file_change(client: &mut LspClient, uri: &str) {
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":uri,"type":2}]}),
        );
    }

    fn configure_counting_compiler(client: &mut LspClient, compiler: &Path) {
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":compiler.to_string_lossy(),
                "compilerArguments":[]
            }}}),
        );
    }

    fn assert_compiler_runs(path: &Path, expected: usize, scenario: &str) {
        assert_eq!(compiler_run_count(path), expected, "{scenario}");
    }

    fn process_is_running(pid: &str) -> bool {
        if let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some((_, fields)) = stat.rsplit_once(") ")
        {
            return fields
                .split_whitespace()
                .next()
                .is_some_and(|state| state != "Z" && state != "X");
        }
        Command::new("/bin/kill")
            .arg("-0")
            .arg(pid)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn fixing_a_closed_import_clears_its_published_diagnostic() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let main_text = "import dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        fs::write(&main_path, main_text).must_be("write root source");
        fs::write(&dependency_path, "BAD\n").must_be("write broken dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();

        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":main_uri,
                "languageId":"bend",
                "version":1,
                "text":main_text
            }}),
        );
        assert!(diagnostics_for(
            &mut client,
            &dependency_uri,
            false,
            Duration::from_secs(5)
        ));

        fs::write(&dependency_path, "def value: Type\n  1\n").must_be("fix dependency on disk");
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":dependency_uri,"type":2}]}),
        );
        assert!(
            diagnostics_for(&mut client, &dependency_uri, true, Duration::from_secs(2)),
            "fixing a closed imported file must publish an empty diagnostic list for that URI"
        );
        client.finish();
    }

    #[test]
    fn shared_import_diagnostic_remains_until_every_root_is_clean() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let first_path = workspace.join("first.bend");
        let second_path = workspace.join("second.bend");
        let dependency_path = workspace.join("dep.bend");
        let importing_text = "import dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        let clean_text = "def main: Type\n  1\n";
        fs::write(&first_path, importing_text).must_be("write first root");
        fs::write(&second_path, importing_text).must_be("write second root");
        fs::write(&dependency_path, "BAD\n").must_be("write broken dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let first_uri = Url::from_file_path(&first_path)
            .must_be("valid test fixture value")
            .to_string();
        let second_uri = Url::from_file_path(&second_path)
            .must_be("valid test fixture value")
            .to_string();
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();

        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        for uri in [&first_uri, &second_uri] {
            client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":uri,
                    "languageId":"bend",
                    "version":1,
                    "text":importing_text
                }}),
            );
        }
        assert!(diagnostics_for(
            &mut client,
            &dependency_uri,
            false,
            Duration::from_secs(5)
        ));
        let second_checked = client.receive_matching(Duration::from_secs(5), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == second_uri
                && message["params"]["version"] == 1
        });
        assert!(
            second_checked.is_some(),
            "second root analysis did not finish"
        );

        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":first_uri,"version":2},"contentChanges":[{"text":clean_text}]}),
        );
        let first_checked = client.receive_matching(Duration::from_secs(5), |message| {
            message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == first_uri
                && message["params"]["version"] == 2
        });
        assert!(
            first_checked.is_some(),
            "updated first root analysis did not finish"
        );
        assert!(
            !diagnostics_for(
                &mut client,
                &dependency_uri,
                true,
                Duration::from_millis(500)
            ),
            "one clean root must not clear another root's imported diagnostic"
        );

        fs::write(&dependency_path, "def value: Type\n  1\n").must_be("fix shared dependency");
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":dependency_uri,"type":2}]}),
        );
        assert!(diagnostics_for(
            &mut client,
            &dependency_uri,
            true,
            Duration::from_secs(2)
        ));
        client.finish();
    }
    #[test]
    fn document_symbols_expose_top_level_declarations_and_constructors() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ntype Shape is Data:\n  Circle{r: U32}\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();

        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let response = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        let symbols = response["result"]
            .as_array()
            .must_be("documentSymbol must return symbols");
        assert!(
            symbols.iter().any(|symbol| symbol["name"] == "add"),
            "missing function symbol: {symbols:?}"
        );
        let shape = symbols
            .iter()
            .find(|symbol| symbol["name"] == "Shape")
            .must_be("missing type symbol");
        assert!(
            shape["children"]
                .as_array()
                .is_some_and(|children| children.iter().any(|symbol| symbol["name"] == "Circle")),
            "type symbol must own its constructors: {shape:?}"
        );
        client.finish();
    }
    #[test]
    fn hover_on_a_function_name_shows_its_signature() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source =
            "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  add(1, 2)\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let response = client.request(
            "textDocument/hover",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":4}
            }),
        );
        assert!(
            response["result"]["contents"]["value"]
                .as_str()
                .is_some_and(|value| value.contains("def add(x: U32, y: U32) -> U32")),
            "hover must expose the declaration signature: {response}"
        );
        let definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":4}
            }),
        );
        assert_eq!(
            definition["result"]["range"]["start"]["line"], 0,
            "local function navigation must resolve to its declaration: {definition}"
        );
        client.finish();
    }
    #[test]
    fn type_definition_resolves_an_annotation_to_its_type() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "type Shape is Data:\n  Circle{}\ndef consume(item: Shape) -> U32:\n  1\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let response = client.request(
            "textDocument/typeDefinition",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":2,"character":19}
            }),
        );
        assert_eq!(
            response["result"]["range"]["start"]["line"], 0,
            "type definition must resolve the annotation: {response}"
        );
        client.finish();
    }
    #[test]
    fn completion_offers_matching_local_functions() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  ad\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let response = client.request(
            "textDocument/completion",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":4}
            }),
        );
        assert!(
            response["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["label"] == "add")),
            "completion must include declarations matching the current prefix: {response}"
        );
        client.finish();
    }
    #[test]
    fn immediate_hover_observes_the_latest_did_change_revision() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let initial = "def revision_one: U32\n  1\n";
        fs::write(&main_path, initial).must_be("write initial source");
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        open_clean_document(&mut client, &uri, initial);

        let large = include_str!("../benches/fixtures/analyzer_large.bend");
        let revised = format!("def revision_two: U32\n  2\n{large}");
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":revised}]}),
        );
        let hover = client.request(
            "textDocument/hover",
            json!({"textDocument":{"uri":uri},"position":{"line":0,"character":5}}),
        );
        let value = hover["result"]["contents"]["value"]
            .as_str()
            .must_be("hover result for revision two");
        assert!(
            value.contains("def revision_two: U32") && !value.contains("revision_one"),
            "hover after didChange(v2) must not read v1: {hover}"
        );
        client.finish();
    }
    #[test]
    fn immediate_document_symbol_and_completion_observe_latest_revision() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let initial = "def revision_one: U32\n  1\n";
        fs::write(&main_path, initial).must_be("write initial source");
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        open_clean_document(&mut client, &uri, initial);

        let revised = "def revision_two: U32\n  2\ndef caller: U32\n  revi\n";
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":revised}]}),
        );
        let requests = [
            (
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":uri}}),
            ),
            (
                "textDocument/completion",
                json!({"textDocument":{"uri":uri},"position":{"line":3,"character":6}}),
            ),
        ];
        let responses = request_burst_results(&mut client, &requests);
        assert!(
            responses.iter().any(|(response, _)| {
                response["result"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["name"] == "revision_two"))
            }),
            "document symbols after didChange(v2) must expose revision_two: {responses:?}"
        );
        assert!(
            responses.iter().any(|(response, _)| {
                response["result"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["label"] == "revision_two"))
            }),
            "completion after didChange(v2) must use v2 declarations: {responses:?}"
        );
        client.finish();
    }
    #[test]
    fn superseded_did_change_cannot_overwrite_newer_revision() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let initial = "def revision_one: U32\n  1\n";
        fs::write(&main_path, initial).must_be("write initial source");
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        open_clean_document(&mut client, &uri, initial);

        let large = include_str!("../benches/fixtures/analyzer_large.bend");
        let revision_two = format!("def revision_two: U32\n  2\n{large}");
        let revision_three = format!("def revision_three: U32\n  3\n{large}");
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":revision_two}]}),
        );
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":3},"contentChanges":[{"text":revision_three}]}),
        );
        let hover = client.request(
            "textDocument/hover",
            json!({"textDocument":{"uri":uri},"position":{"line":0,"character":5}}),
        );
        let value = hover["result"]["contents"]["value"]
            .as_str()
            .must_be("hover result for revision three");
        assert!(
            value.contains("def revision_three: U32") && !value.contains("revision_two"),
            "superseded v2 must never replace v3: {hover}"
        );
        client.finish();
    }
    const PENDING_IMPORT_CALLS: usize = 1_200;

    fn open_pending_importer_fixture() -> (tempfile::TempDir, LspClient, String, String) {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let importer_path = workspace.join("main.bend");
        let module_path = workspace.join("dep.bend");
        let module = "def clamp(x: U32) -> U32:\n  x\n";
        let mut importer = String::from("import dep.bend as Dep\n");
        for index in 0..PENDING_IMPORT_CALLS {
            write!(importer, "def use_{index}: U32\n  Dep.clamp(1)\n")
                .must_be("append importing call");
        }
        fs::write(&importer_path, &importer).must_be("write importing source");
        fs::write(&module_path, module).must_be("write declaration module");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let importer_uri = Url::from_file_path(&importer_path)
            .must_be("valid importer URI")
            .to_string();
        let module_uri = Url::from_file_path(&module_path)
            .must_be("valid module URI")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":importer_uri,"languageId":"bend","version":1,"text":importer
            }}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":module_uri,"languageId":"bend","version":1,"text":module
            }}),
        );
        (temp, client, importer_uri, module_uri)
    }

    #[test]
    fn immediate_workspace_symbols_include_preceding_pending_importer() {
        let (_temp, mut client, importer_uri, _module_uri) = open_pending_importer_fixture();
        let response = client.request("workspace/symbol", json!({"query":"use_1199"}));
        assert!(
            response["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| {
                    item["name"] == "use_1199" && item["location"]["uri"] == importer_uri
                })),
            "immediate workspace symbols must include the pending importer's declaration: {response}"
        );
        client.finish();
    }

    #[test]
    fn immediate_incoming_calls_include_preceding_pending_importer() {
        let (_temp, mut client, importer_uri, module_uri) = open_pending_importer_fixture();
        let item = json!({
            "name":"clamp",
            "kind":12,
            "uri":module_uri,
            "range":{"start":{"line":0,"character":0},"end":{"line":1,"character":3}},
            "selectionRange":{"start":{"line":0,"character":4},"end":{"line":0,"character":9}}
        });
        let response = client.request("callHierarchy/incomingCalls", json!({"item":item}));
        let callers = response["result"]
            .as_array()
            .must_be("incoming calls must return an array");
        assert_eq!(
            callers.len(),
            PENDING_IMPORT_CALLS,
            "immediate incoming calls must include each pending caller: {response}"
        );
        assert!(
            callers
                .iter()
                .all(|call| call["from"]["uri"] == importer_uri)
        );
        client.finish();
    }

    #[test]
    fn immediate_outgoing_calls_resolve_preceding_pending_target_open() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let caller_path = workspace.join("main.bend");
        let target_path = workspace.join("dep.bend");
        let caller = "import dep.bend as Dep\ndef main: U32\n  Dep.clamp(1)\n";
        fs::write(&caller_path, caller).must_be("write caller");
        fs::write(&target_path, "def old(x: U32) -> U32:\n  x\n").must_be("write old target");
        let caller_uri = Url::from_file_path(&caller_path)
            .must_be("valid caller URI")
            .to_string();
        let target_uri = Url::from_file_path(&target_path)
            .must_be("valid target URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":caller_uri,"languageId":"bend","version":1,"text":caller}}),
        );
        let prepared = client.request(
            "textDocument/prepareCallHierarchy",
            json!({"textDocument":{"uri":caller_uri},"position":{"line":1,"character":5}}),
        );
        let caller_item = prepared["result"][0].clone();
        assert_eq!(
            caller_item["name"], "main",
            "caller must be prepared: {prepared}"
        );
        let mut target = String::new();
        for index in 0..PENDING_IMPORT_CALLS {
            write!(target, "def slow_{index}: U32\n  1\n").must_be("append pending target");
        }
        target.push_str("def clamp(x: U32) -> U32:\n  x\n");
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":target_uri,"languageId":"bend","version":1,"text":target}}),
        );
        let response = client.request("callHierarchy/outgoingCalls", json!({"item":caller_item}));
        assert!(
            response["result"].as_array().is_some_and(|calls| calls
                .iter()
                .any(|call| { call["to"]["name"] == "clamp" && call["to"]["uri"] == target_uri })),
            "outgoing calls must resolve the accepted target update: {response}"
        );
        client.finish();
    }

    #[test]
    fn immediate_references_include_preceding_pending_importer() {
        let (_temp, mut client, importer_uri, module_uri) = open_pending_importer_fixture();
        let params = json!({
            "textDocument":{"uri":module_uri},
            "position":{"line":0,"character":6},
            "context":{"includeDeclaration":true}
        });
        let immediate = client.request("textDocument/references", params.clone());
        let check = |response: &Value| {
            let locations = response["result"]
                .as_array()
                .must_be("references must be a location array");
            assert_eq!(
                locations.len(),
                PENDING_IMPORT_CALLS + 1,
                "references must include every preceding open document"
            );
            assert!(locations.iter().any(|location| {
                location["uri"] == module_uri
                    && location["range"]["start"] == json!({"line":0,"character":4})
                    && location["range"]["end"] == json!({"line":0,"character":9})
            }));
            let call_lines = locations
                .iter()
                .filter(|location| location["uri"] == importer_uri)
                .map(|location| {
                    assert_eq!(location["range"]["start"]["character"], 6);
                    assert_eq!(location["range"]["end"]["character"], 11);
                    location["range"]["start"]["line"]
                        .as_u64()
                        .must_be("call line number")
                })
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                call_lines,
                (0..PENDING_IMPORT_CALLS)
                    .map(|index| 2 + 2 * index as u64)
                    .collect()
            );
        };
        check(&immediate);
        check(&client.request("textDocument/references", params));
        client.finish();
    }

    #[test]
    fn immediate_rename_includes_preceding_pending_importer() {
        let (_temp, mut client, importer_uri, module_uri) = open_pending_importer_fixture();
        let response = client.request(
            "textDocument/rename",
            json!({
                "textDocument":{"uri":module_uri},
                "position":{"line":0,"character":6},
                "newName":"limit"
            }),
        );
        let changes = response["result"]["changes"]
            .as_object()
            .must_be("rename must produce workspace edits");
        assert_eq!(changes.len(), 2);
        let importer_edits = changes[&importer_uri]
            .as_array()
            .must_be("rename must edit importing document");
        assert_eq!(importer_edits.len(), PENDING_IMPORT_CALLS);
        assert!(importer_edits.iter().all(|edit| edit["newText"] == "limit"));
        let declaration_edits = changes[&module_uri]
            .as_array()
            .must_be("rename must edit declaration module");
        assert_eq!(declaration_edits.len(), 1);
        assert_eq!(
            declaration_edits[0]["range"]["start"],
            json!({"line":0,"character":4})
        );
        client.finish();
    }

    #[test]
    fn references_and_rename_cover_declaration_and_call_sites() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def add(x: U32) -> U32:\n  x\ndef main: U32\n  add(1)\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let references = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":4},
                "context":{"includeDeclaration":true}
            }),
        );
        assert_eq!(
            references["result"].as_array().map(Vec::len),
            Some(2),
            "references must include the definition and use: {references}"
        );
        let rename = client.request(
            "textDocument/rename",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":4},
                "newName":"sum"
            }),
        );
        assert_eq!(
            rename["result"]["changes"][uri].as_array().map(Vec::len),
            Some(2),
            "rename must edit the declaration and call: {rename}"
        );
        client.finish();
    }
    #[test]
    fn workspace_symbols_filter_open_document_declarations() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def add(x: U32) -> U32:\n  x\ntype Shape is Data:\n  Circle{}\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let document_symbols = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            document_symbols["result"]
                .as_array()
                .is_some_and(|symbols| symbols.iter().any(|symbol| symbol["name"] == "add")),
            "open document symbols must be committed before workspace-wide search"
        );
        let response = client.request("workspace/symbol", json!({"query":"add"}));
        assert!(
            response["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == "add")),
            "workspace symbols must find matching top-level declarations: {response}"
        );
        client.finish();
    }
    #[test]
    fn incremental_change_preserves_unedited_source() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main: U32\n  1\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":uri,"version":2},
                "contentChanges":[{
                    "range":{
                        "start":{"line":0,"character":0},
                        "end":{"line":0,"character":0}
                    },
                    "text":"# prefix\n"
                }]
            }),
        );
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":uri,"version":3},
                "contentChanges":[{
                    "range":{
                        "start":{"line":1,"character":4},
                        "end":{"line":1,"character":8}
                    },
                    "text":"new"
                }]
            }),
        );
        let response = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            response["result"].as_array().is_some_and(|items| {
                items.iter().any(|item| item["name"] == "new")
                    && !items.iter().any(|item| item["name"] == "old")
            }),
            "the second incremental edit must use the revised line index: {response}"
        );
        client.finish();
    }
    #[test]
    fn exit_notification_terminates_server_without_stdin_eof() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        let response = client.request("shutdown", Value::Null);
        assert!(
            response.get("error").is_none(),
            "shutdown failed: {response}"
        );
        client.notify("exit", Value::Null);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut exited = false;
        while Instant::now() < deadline {
            if client.try_wait().is_some() {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            exited,
            "exit notification must terminate without closing stdin"
        );
    }
    #[test]
    fn untitled_documents_support_symbols_and_completion() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let source = "def value: U32\n  1\ndef main: U32\n  val\n";
        let uri = "untitled:scratch.bend";
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let symbols = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            symbols["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == "value")),
            "untitled buffers must receive source features: {symbols}"
        );
        let completion = client.request(
            "textDocument/completion",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":5}
            }),
        );
        assert!(
            completion["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["label"] == "value")),
            "untitled buffers must receive contextual completion: {completion}"
        );
        client.finish();
    }
    #[test]
    fn signature_help_selects_the_active_parameter() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source =
            "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  add(1, 2)\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let response = client.request(
            "textDocument/signatureHelp",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":9}
            }),
        );
        assert!(
            response["result"]["signatures"][0]["label"]
                .as_str()
                .is_some_and(|label| label.contains("add(x: U32, y: U32)")),
            "signature help must show the call target's declaration: {response}"
        );
        assert_eq!(
            response["result"]["activeParameter"], 1,
            "signature help must select the second argument: {response}"
        );
        client.finish();
    }
    #[test]
    fn semantic_tokens_classify_declaration_names() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def echo(value: U32) -> String:\n  \"🦀\" # comment\n  42\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let response = client.request(
            "textDocument/semanticTokens/full",
            json!({"textDocument":{"uri":uri}}),
        );
        let data = response["result"]["data"]
            .as_array()
            .must_be("semantic token data");
        let mut line = 0;
        let mut character = 0;
        let tokens: Vec<(u32, u32, u32, u32)> = data
            .as_chunks::<5>()
            .0
            .iter()
            .map(|token| {
                line += semantic_token_number(&token[0]);
                character = if token[0] == 0 {
                    character + semantic_token_number(&token[1])
                } else {
                    semantic_token_number(&token[1])
                };
                (
                    line,
                    character,
                    semantic_token_number(&token[2]),
                    semantic_token_number(&token[3]),
                )
            })
            .collect();
        for expected in [
            (0, 4, 4, 12),
            (0, 9, 5, 8),
            (0, 16, 3, 1),
            (1, 2, 4, 18),
            (1, 7, 9, 17),
            (2, 2, 2, 19),
        ] {
            assert!(
                tokens.contains(&expected),
                "missing semantic token {expected:?}: {tokens:?}"
            );
        }
        client.finish();
    }
    fn request_delimiter_action(
        client: &mut LspClient,
        uri: &str,
        requested_range: &Value,
        only: Option<&str>,
    ) -> Value {
        let mut context = json!({"diagnostics":[{
            "range":{
                "start":{"line":1,"character":2},
                "end":{"line":1,"character":3}
            },
            "severity":1,
            "code":"parsing",
            "source":"bend2",
            "message":"Unclosed '('."
        }]});
        if let Some(only) = only {
            context["only"] = json!([only]);
        }
        client.request(
            "textDocument/codeAction",
            json!({
                "textDocument":{"uri":uri},
                "range":requested_range,
                "context":context
            }),
        )
    }

    #[test]
    fn code_action_offers_closing_a_reported_unclosed_delimiter() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main: U32\n  (1 # trailing comment\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let response = request_delimiter_action(
            &mut client,
            &uri,
            &json!({
                "start":{"line":1,"character":2},
                "end":{"line":1,"character":3}
            }),
            None,
        );
        assert!(
            response["result"]
                .as_array()
                .is_some_and(|actions| actions.iter().any(|action| {
                    action["title"] == "Insert ')'"
                        && action["edit"]["changes"][uri.as_str()][0]["newText"] == ")"
                        && action["edit"]["changes"][uri.as_str()][0]["range"]["start"]
                            == json!({"line":1,"character":4})
                        && action["edit"]["changes"][uri.as_str()][0]["range"]["end"]
                            == json!({"line":1,"character":4})
                })),
            "an unclosed-delimiter diagnostic must offer a safe insertion fix: {response}"
        );
        let outside_range = request_delimiter_action(
            &mut client,
            &uri,
            &json!({
                "start":{"line":0,"character":0},
                "end":{"line":0,"character":10}
            }),
            None,
        );
        assert_eq!(
            outside_range["result"].as_array().map(Vec::len),
            Some(0),
            "a quick fix must not be offered outside the requested range: {outside_range}"
        );
        let filtered = request_delimiter_action(
            &mut client,
            &uri,
            &json!({
                "start":{"line":1,"character":2},
                "end":{"line":1,"character":3}
            }),
            Some("source"),
        );
        assert_eq!(
            filtered["result"].as_array().map(Vec::len),
            Some(0),
            "a non-quickfix kind filter must suppress quick fixes: {filtered}"
        );
        client.finish();
    }
    #[test]
    fn folding_selection_and_document_links_follow_source_ranges() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let source = "import ./dep.bend as Dep\ndef main: U32\n  Dep.value\n";
        fs::write(&main_path, source).must_be("write source");
        fs::write(&dependency_path, "def value: U32\n  1\n").must_be("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let folding = client.request(
            "textDocument/foldingRange",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            folding["result"].as_array().is_some_and(|ranges| ranges
                .iter()
                .any(|range| { range["startLine"] == 1 && range["endLine"] == 2 })),
            "multi-line function bodies must be foldable: {folding}"
        );
        let selection = client.request(
            "textDocument/selectionRange",
            json!({
                "textDocument":{"uri":uri},
                "positions":[{"line":2,"character":7}]
            }),
        );
        assert_eq!(
            selection["result"][0]["range"]["start"]["character"], 6,
            "selection must begin with the word under the cursor: {selection}"
        );
        let links = client.request(
            "textDocument/documentLink",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            links["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["target"] == dependency_uri)),
            "relative import paths must link to their target: {links}"
        );
        client.finish();
    }
    #[test]
    fn range_and_on_type_formatting_return_local_edits() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def first : U32:\n    1\ndef second: U32\n    2\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let ranged = client.request(
            "textDocument/rangeFormatting",
            json!({
                "textDocument":{"uri":uri},
                "range":{
                    "start":{"line":0,"character":0},
                    "end":{"line":1,"character":5}
                },
                "options":{"tabSize":2,"insertSpaces":true}
            }),
        );
        assert!(
            ranged["result"].as_array().is_some_and(|edits| {
                edits.len() == 1
                    && edits[0]["range"]["start"] == json!({"line":0,"character":0})
                    && edits[0]["range"]["end"] == json!({"line":1,"character":5})
                    && edits[0]["newText"] == "def first: U32:\n  1"
            }),
            "range formatting must normalize only the selected function: {ranged}"
        );
        let on_type = client.request(
            "textDocument/onTypeFormatting",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":1,"character":0},
                "ch":"\n",
                "options":{"tabSize":2,"insertSpaces":true}
            }),
        );
        assert!(
            on_type["result"]
                .as_array()
                .is_some_and(|edits| edits.first().is_some_and(|edit| edit["newText"] == "  ")),
            "newline formatting must align the current body line: {on_type}"
        );
        client.finish();
    }
    #[test]
    fn inlay_hints_name_call_arguments_and_lenses_count_references() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source =
            "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  add(1, 2)\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let range = json!({
            "start":{"line":0,"character":0},
            "end":{"line":3,"character":10}
        });
        let hints = client.request(
            "textDocument/inlayHint",
            json!({"textDocument":{"uri":uri},"range":range}),
        );
        assert!(
            hints["result"].as_array().is_some_and(|items| {
                items.iter().any(|item| item["label"] == "x: ")
                    && items.iter().any(|item| item["label"] == "y: ")
            }),
            "positional call arguments should show their parameter names: {hints}"
        );
        let lenses = client.request("textDocument/codeLens", json!({"textDocument":{"uri":uri}}));
        assert!(
            lenses["result"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["range"]["start"]["line"] == 0 && item["command"]["title"] == "1 reference"
                })
            }),
            "function lenses should expose reference counts: {lenses}"
        );
        client.finish();
    }
    #[test]
    fn parameter_hover_and_type_navigation_use_declared_type() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "type Shape is Data:\n  Circle{}\ndef consume(item: Shape) -> U32:\n  item\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let position = json!({"textDocument":{"uri":uri},"position":{"line":3,"character":4}});
        let hover = client.request("textDocument/hover", position.clone());
        assert!(
            hover["result"]["contents"]["value"]
                .as_str()
                .is_some_and(|value| value.contains("item: Shape")),
            "parameter references should show their inferred annotation: {hover}"
        );
        let definition = client.request("textDocument/typeDefinition", position);
        assert_eq!(
            definition["result"]["range"]["start"]["line"], 0,
            "type navigation on a parameter should reach its declared type: {definition}"
        );
        client.finish();
    }
    #[test]
    fn watched_import_changes_recheck_open_dependents() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let source = "import Base # builtin prelude\nimport ./dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        fs::write(&main_path, source).must_be("write source");
        fs::write(&dependency_path, "def value: Type\n  1\n").must_be("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":main_uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        assert!(diagnostics_for(
            &mut client,
            &main_uri,
            true,
            Duration::from_secs(5)
        ));
        fs::write(&dependency_path, "BAD\n").must_be("break dependency");
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":dependency_uri,"type":2}]}),
        );
        assert!(
            diagnostics_for(&mut client, &dependency_uri, false, Duration::from_secs(5)),
            "a watched imported-file change must recheck and publish diagnostics: {dependency_uri}"
        );
        client.finish();
    }
    #[test]
    fn advertises_workspace_folders_and_registers_bend_file_watcher() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let root_uri = Url::from_directory_path(&workspace).must_be("valid test fixture value");
        let mut client = spawn_client(&compiler_dir);
        let initialized = client.request(
            "initialize",
            json!({
                "processId":null,
                "rootUri":root_uri,
                "workspaceFolders":[{"uri":root_uri,"name":"test"}],
                "capabilities":{
                    "workspace":{
                        "didChangeWatchedFiles":{"dynamicRegistration":true}
                    }
                }
            }),
        );
        assert_eq!(
            initialized["result"]["capabilities"]["workspace"]["workspaceFolders"]["supported"],
            true
        );
        assert_eq!(
            initialized["result"]["capabilities"]["workspace"]["workspaceFolders"]["changeNotifications"],
            true
        );
        client.notify("initialized", json!({}));
        let registration = client
            .receive_matching(Duration::from_secs(5), |message| {
                message["method"] == "client/registerCapability"
            })
            .must_be("server should register Bend file watching");
        assert_eq!(
            registration["params"]["registrations"][0]["method"],
            "workspace/didChangeWatchedFiles"
        );
        assert_eq!(
            registration["params"]["registrations"][0]["registerOptions"]["watchers"][0]["globPattern"],
            "**/*.bend"
        );
        client.send(&json!({
            "jsonrpc":"2.0",
            "id":registration["id"],
            "result":null
        }));
        client.finish();
    }
    #[test]
    fn configuration_selects_compiler_and_rechecks_open_documents() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main: Type\n  1\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let custom_compiler = temp.path().join("configured-bend");
        fs::write(
            &custom_compiler,
            "#!/bin/sh\nprintf 'run\\n' >> \"$0.runs\"\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"--report-error\" ]; then\n    printf 'Error:\\nconfigured compiler error\\nLocation:\\n1>| def main: Type\\n' >&2\n    exit 1\n  fi\ndone\nexit 0\n",
        ).must_be("write custom compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .must_be("make custom compiler executable");
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":custom_compiler,
                "compilerArguments":[],
                "unknownSetting":true
            }}}),
        );
        assert!(
            client
                .receive_matching(Duration::from_secs(5), |message| {
                    message["method"] == "window/logMessage"
                        && message["params"]["message"]
                            .as_str()
                            .is_some_and(|text| text.contains("unknownSetting"))
                })
                .is_some(),
            "unknown configuration must be reported to the client"
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        wait_for_clean_diagnostics(&mut client, &uri);
        let compiler_runs = custom_compiler.with_extension("runs");
        assert_eq!(compiler_run_count(&compiler_runs), 1);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":custom_compiler,
                "compilerArguments":[]
            }}}),
        );
        wait_for_clean_diagnostics(&mut client, &uri);
        assert_eq!(
            compiler_run_count(&compiler_runs),
            1,
            "rechecking the same source graph and compiler config must reuse cached diagnostics"
        );
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":custom_compiler,
                "compilerArguments":["--report-error"]
            }}}),
        );
        assert!(
            client
                .receive_matching(Duration::from_secs(5), |message| {
                    message["method"] == "textDocument/publishDiagnostics"
                        && message["params"]["uri"] == uri
                        && message["params"]["diagnostics"]
                            .as_array()
                            .is_some_and(|items| {
                                items.iter().any(|item| {
                                    item["message"].as_str().is_some_and(|text| {
                                        text.contains("configured compiler error")
                                    })
                                })
                            })
                })
                .is_some(),
            "configuration changes must recheck documents with the configured compiler"
        );
        assert_eq!(
            compiler_run_count(&compiler_runs),
            2,
            "changing compiler arguments must invalidate the cached result"
        );
        client.finish();
    }
    #[test]
    fn missing_configured_compiler_publishes_actionable_diagnostic() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main: Type\n  1\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let missing_compiler = workspace.join("missing-bend");
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":missing_compiler,
                "compilerArguments":[]
            }}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let diagnostics = client
            .receive_matching(Duration::from_secs(5), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && message["params"]["diagnostics"]
                        .as_array()
                        .is_some_and(|items| {
                            items
                                .iter()
                                .any(|item| item["code"] == "compiler-unavailable")
                        })
            })
            .must_be("missing compiler should be visible to the editor");
        assert!(
            diagnostics["params"]["diagnostics"][0]["message"]
                .as_str()
                .is_some_and(|message| message.contains("Unable to run Bend compiler")),
            "compiler failure should explain the missing executable: {diagnostics}"
        );
        client.finish();
    }
    #[test]
    fn untitled_imports_resolve_from_workspace_without_materializing_source() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let dependency_path = workspace.join("dep.bend");
        fs::write(&dependency_path, "def value: Type\n  1\n").must_be("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = "untitled:scratch.bend";
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();
        let source =
            "import Base # prelude\nimport ./dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        assert!(diagnostics_for(
            &mut client,
            uri,
            true,
            Duration::from_secs(5)
        ));
        fs::write(&dependency_path, "BAD\n").must_be("break dependency");
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":dependency_uri,"type":2}]}),
        );
        assert!(
            diagnostics_for(&mut client, &dependency_uri, false, Duration::from_secs(5)),
            "untitled importers must be rechecked when workspace files change"
        );
        assert!(
            fs::read_dir(&workspace)
                .must_be("workspace directory")
                .all(|entry| !entry
                    .must_be("workspace directory entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".bend2-lsp-virtual-")),
            "virtual source staging must not create files in the workspace"
        );
        client.finish();
    }
    #[test]
    fn workspace_folder_removal_updates_untitled_import_root() {
        let temp = tempdir().must_be("temporary workspace");
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        fs::create_dir_all(&first_root).must_be("create first workspace root");
        fs::create_dir_all(&second_root).must_be("create second workspace root");
        let dependency_path = second_root.join("dep.bend");
        fs::write(&dependency_path, "def value: Type\n  1\n").must_be("write dependency");
        let first_uri = Url::from_directory_path(&first_root).must_be("valid test fixture value");
        let second_uri = Url::from_directory_path(&second_root).must_be("valid test fixture value");
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        let initialized = client.request(
            "initialize",
            json!({
                "processId":null,
                "rootUri":first_uri,
                "workspaceFolders":[
                    {"uri":first_uri,"name":"first"},
                    {"uri":second_uri,"name":"second"}
                ],
                "capabilities":{}
            }),
        );
        assert!(initialized["result"]["capabilities"].is_object());
        client.notify("initialized", json!({}));
        let uri = "untitled:scratch.bend";
        let source = "import ./dep.bend as Dep\ndef main() -> U32:\n  Dep.value\n";
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let position = json!({
            "textDocument":{"uri":uri},
            "position":{"line":2,"character":6}
        });
        assert!(
            client.request("textDocument/definition", position.clone())["result"].is_null(),
            "untitled imports initially resolve from the first workspace folder"
        );
        client.notify(
            "workspace/didChangeWorkspaceFolders",
            json!({"event":{
                "added":[],
                "removed":[{"uri":first_uri,"name":"first"}]
            }}),
        );
        assert_eq!(
            client.request("textDocument/definition", position)["result"]["uri"],
            dependency_uri,
            "removing the first folder must promote the second root for open untitled documents"
        );
        client.finish();
    }

    fn open_prelude_document() -> (tempfile::TempDir, LspClient) {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let custom_compiler = compiler_dir.join("bend");
        fs::write(
            &custom_compiler,
            "#!/bin/sh\nif [ \"$1\" = base ]; then\n  printf 'type Builtin is Data:\\n  Builtin{}\\ndef List.map(x: U32) -> U32:\\n  x\\n'\n  exit 0\nfi\nexit 0\n",
        ).must_be("write compiler with Base source");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .must_be("make compiler executable");
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":"untitled:prelude.bend",
                "languageId":"bend",
                "version":1,
                "text":"import Base\ndef main() -> Builtin:\n  List.map\n"
            }}),
        );
        (temp, client)
    }

    #[test]
    fn prelude_symbols_support_hover_navigation_and_completion() {
        let (_temp, mut client) = open_prelude_document();
        let uri = "untitled:prelude.bend";
        let hover = client.request(
            "textDocument/hover",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":2,"character":7}
            }),
        );
        assert!(
            hover["result"]["contents"]["value"]
                .as_str()
                .is_some_and(|value| value.contains("def List.map(x: U32) -> U32")),
            "Base declarations should provide semantic hover: {hover}"
        );
        let definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":2,"character":7}
            }),
        );
        let base_uri = definition["result"]["uri"]
            .as_str()
            .must_be("Base definition URI")
            .to_owned();
        assert!(
            fs::read_to_string(
                Url::parse(&base_uri)
                    .must_be("Base definition URL")
                    .to_file_path()
                    .must_be("Base definition path")
            )
            .must_be("read Base definition source")
            .contains("def List.map(x: U32) -> U32"),
            "Base navigation must point at readable source: {definition}"
        );
        let type_definition = client.request(
            "textDocument/typeDefinition",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":1,"character":17}
            }),
        );
        assert_eq!(
            type_definition["result"]["uri"], base_uri,
            "prelude types should resolve to the readable Base source"
        );
        let symbols = client.request("workspace/symbol", json!({"query":"List.map"}));
        assert!(
            symbols["result"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item["name"] == "List.map" && item["location"]["uri"] == base_uri)
            }),
            "workspace symbol search should include declarations from imported Base: {symbols}"
        );
        let completion = client.request(
            "textDocument/completion",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":2,"character":7}
            }),
        );
        assert!(
            completion["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["label"] == "map")),
            "completion after a Base namespace should offer matching declarations: {completion}"
        );
        let base_completion = client.request(
            "textDocument/completion",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":2,"character":5}
            }),
        );
        assert!(
            base_completion["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["label"] == "List.map")),
            "unqualified completion should include imported Base declarations: {base_completion}"
        );
        client.finish();
    }

    fn generated_base_diagnostics(client: &mut LspClient, uri: &str, version: i32) -> Value {
        client
            .receive_matching(Duration::from_secs(5), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && message["params"]["version"] == version
            })
            .must_be("generated Base revision diagnostics")
    }

    fn open_generated_base_document() -> (tempfile::TempDir, LspClient, PathBuf, String, String) {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let compiler = compiler_dir.join("bend");
        fs::write(
            &compiler,
            "#!/bin/sh\nvalue=7\nif [ \"$1\" = --fresh ]; then value=9; shift; fi\nif [ \"$1\" = base ]; then\n  printf 'def library_value() -> U32:\\n  %s\\n' \"$value\"\n  exit 0\nfi\nif grep -q '^def library_value' \"$1\"; then\n  printf 'Error:\\nstandalone compiler input is not builtin Base\\nLocation:\\n1>| def library_value() -> U32:\\n' >&2\n  exit 1\nfi\nexit 0\n",
        )
        .must_be("write compiler with builtin-only declarations");
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        let root_uri = "untitled:base-consumer.bend";
        open_clean_document(
            &mut client,
            root_uri,
            "import Base\ndef main() -> U32:\n  library_value()\n",
        );
        let definition = client.request(
            "textDocument/definition",
            json!({"textDocument":{"uri":root_uri},"position":{"line":2,"character":7}}),
        );
        let base_uri = definition["result"]["uri"]
            .as_str()
            .must_be("generated Base URI")
            .to_owned();
        let base_path = Url::parse(&base_uri)
            .must_be("generated Base URL")
            .to_file_path()
            .must_be("generated Base path");
        let base_text = fs::read_to_string(&base_path).must_be("read generated Base");
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":base_uri,"languageId":"bend","version":1,"text":base_text}}),
        );
        assert_eq!(
            generated_base_diagnostics(&mut client, &base_uri, 1)["params"]["diagnostics"],
            json!([])
        );
        (temp, client, compiler, base_uri, base_text)
    }

    #[test]
    fn generated_base_preserves_lexical_checks_and_user_file_diagnostics() {
        let (temp, mut client, _, base_uri, base_text) = open_generated_base_document();
        let hover = client.request(
            "textDocument/hover",
            json!({"textDocument":{"uri":base_uri},"position":{"line":0,"character":8}}),
        );
        assert!(
            hover["result"]["contents"]["value"]
                .as_str()
                .is_some_and(|value| value.contains("def library_value() -> U32")),
            "opening builtin source must preserve indexed navigation: {hover}"
        );
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":base_uri,"version":2},"contentChanges":[{
                "text":"def library_value() -> U32:\n  (7\n"
            }]}),
        );
        let lexical = generated_base_diagnostics(&mut client, &base_uri, 2);
        let items = lexical["params"]["diagnostics"]
            .as_array()
            .must_be("lexical diagnostics array");
        assert!(items.iter().all(|item| item["code"] == "parsing"));
        assert!(
            items
                .iter()
                .any(|item| { item["range"]["start"] == json!({"line":1,"character":2}) })
        );
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":base_uri,"version":3},"contentChanges":[{"text":base_text}]}),
        );
        assert_eq!(
            generated_base_diagnostics(&mut client, &base_uri, 3)["params"]["diagnostics"],
            json!([])
        );
        let user_path = temp.path().join("workspace/Base.bend");
        fs::write(&user_path, &base_text).must_be("write user-owned Base");
        let user_uri = Url::from_file_path(user_path).must_be("user Base URI");
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":user_uri,"languageId":"bend","version":1,"text":base_text}}),
        );
        let diagnostic = client
            .receive_matching(Duration::from_secs(5), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == user_uri.as_str()
                    && message["params"]["diagnostics"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(|item| item["code"] == "checking"))
            })
            .must_be("user-owned Base must still receive compiler diagnostics");
        assert_eq!(diagnostic["params"]["version"], 1);
        client.finish();
    }

    #[test]
    fn generated_base_keeps_provenance_across_configuration_change_and_reopen() {
        let (_temp, mut client, compiler, base_uri, base_text) = open_generated_base_document();
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{"compilerPath":compiler,"compilerArguments":["--fresh"]}}}),
        );
        assert_eq!(
            generated_base_diagnostics(&mut client, &base_uri, 1)["params"]["diagnostics"],
            json!([])
        );
        let updated = client.request(
            "textDocument/definition",
            json!({"textDocument":{"uri":"untitled:base-consumer.bend"},"position":{"line":2,"character":7}}),
        );
        let updated_uri = updated["result"]["uri"]
            .as_str()
            .must_be("updated generated Base URI");
        assert_ne!(updated_uri, base_uri);
        let updated_path = Url::parse(updated_uri)
            .must_be("updated Base URL")
            .to_file_path()
            .must_be("updated Base path");
        assert_eq!(
            fs::read_to_string(updated_path).must_be("read updated Base"),
            "def library_value() -> U32:\n  9\n"
        );
        let base_path = Url::parse(&base_uri)
            .must_be("old Base URL")
            .to_file_path()
            .must_be("old Base path");
        assert_eq!(
            fs::read_to_string(&base_path).must_be("old navigation target must remain readable"),
            base_text
        );
        client.notify(
            "textDocument/didClose",
            json!({"textDocument":{"uri":base_uri}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":base_uri,"languageId":"bend","version":1,"text":base_text}}),
        );
        assert_eq!(
            generated_base_diagnostics(&mut client, &base_uri, 1)["params"]["diagnostics"],
            json!([])
        );
        client.finish();
    }

    #[test]
    fn configured_compiler_profile_preserves_prelude_navigation() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let custom_compiler = compiler_dir.join("bend");
        fs::write(
            &custom_compiler,
            "#!/bin/sh\nif [ \"$1\" != --profile=custom ]; then exit 64; fi\nshift\nif [ \"$1\" = base ]; then\n  printf 'def profile_builtin() -> U32:\\n  7\\n'\nfi\nexit 0\n",
        )
        .must_be("write profile-dependent compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .must_be("make compiler executable");
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":custom_compiler,
                "compilerArguments":["--profile=custom"]
            }}}),
        );
        let uri = "untitled:configured-prelude.bend";
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":"import Base\ndef main() -> U32:\n  profile_builtin()\n"
            }}),
        );
        let definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":2,"character":7}
            }),
        );
        assert_eq!(
            definition["result"]["range"],
            json!({
                "start":{"line":0,"character":4},
                "end":{"line":0,"character":19}
            }),
            "configured compiler profiles must expose their Base declarations: {definition}"
        );
        let base_path = Url::parse(
            definition["result"]["uri"]
                .as_str()
                .must_be("configured Base definition URI"),
        )
        .must_be("configured Base definition URL")
        .to_file_path()
        .must_be("configured Base definition path");
        assert_eq!(
            fs::read_to_string(base_path)
                .must_be("read configured Base definition")
                .lines()
                .next(),
            Some("def profile_builtin() -> U32:"),
            "navigation must lead to the declaration selected by the compiler profile"
        );
        client.finish();
    }

    #[test]
    fn cached_hub_imports_support_navigation_and_workspace_indexing() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        let bend_lib = temp.path().join("bend-lib");
        let hash = "0123456789abcdef0123456789abcdef";
        let package = bend_lib.join(format!("0x{hash}"));
        let names = bend_lib.join("names");
        fs::create_dir_all(&workspace).must_be("create workspace");
        fs::create_dir_all(&package).must_be("create cached Hub package");
        fs::create_dir_all(&names).must_be("create named-package cache");
        let dependency_path = package.join("main.bend");
        fs::write(&dependency_path, "def exported(arg: U32) -> U32:\n  arg\n")
            .must_be("write cached Hub module");
        fs::write(names.join("sample@1.0.0.0"), format!("0x{hash}\n"))
            .must_be("write cached Hub name mapping");
        let main_path = workspace.join("main.bend");
        let source = format!(
            "import 0x{hash}/main.bend as Hash\nimport sample@1.0.0.0/main.bend as P\ndef main() -> U32:\n  P.exported(1)\n"
        );
        fs::write(&main_path, &source).must_be("write root source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_with_bend_lib(&compiler_dir, &bend_lib);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":main_uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let position = json!({
            "textDocument":{"uri":main_uri},
            "position":{"line":3,"character":5}
        });
        let symbols = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":main_uri}}),
        );
        assert!(
            symbols["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == "main")),
            "the opened root document should be available before semantic hover: {symbols}"
        );
        let hover = client.request("textDocument/hover", position.clone());
        assert!(
            hover["result"]["contents"]["value"]
                .as_str()
                .is_some_and(|value| value.contains("def exported(arg: U32) -> U32")),
            "cached Hub symbols should provide semantic hover: {hover}"
        );
        let definition = client.request("textDocument/definition", position.clone());
        assert_eq!(definition["result"]["uri"], dependency_uri);
        let links = client.request(
            "textDocument/documentLink",
            json!({"textDocument":{"uri":main_uri}}),
        );
        assert!(
            links["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["target"] == dependency_uri)),
            "cached Hub imports should expose navigable document links: {links}"
        );
        let references = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":main_uri},
                "position":{"line":3,"character":5},
                "context":{"includeDeclaration":true}
            }),
        );
        assert!(
            references["result"].as_array().is_some_and(|locations| {
                locations.iter().any(|location| location["uri"] == main_uri)
                    && locations
                        .iter()
                        .any(|location| location["uri"] == dependency_uri)
            }),
            "Hub references should include both the call and cached declaration: {references}"
        );
        let symbols = client.request("workspace/symbol", json!({"query":"exported"}));
        assert!(
            symbols["result"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["name"] == "exported" && item["location"]["uri"] == dependency_uri
                })
            }),
            "workspace indexing should include cached Hub modules: {symbols}"
        );
        client.finish();
    }

    #[test]
    fn imported_symbols_support_hover_navigation_and_member_completion() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let source = "import Base # prelude\nimport ./dep.bend as Dep\ndef main() -> U32:\n  Dep.exported(1)\n";
        fs::write(&main_path, source).must_be("write root source");
        fs::write(&dependency_path, "def exported(arg: U32) -> U32:\n  arg\n")
            .must_be("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":main_uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let position = json!({
            "textDocument":{"uri":main_uri},
            "position":{"line":3,"character":9}
        });
        let hover = client.request("textDocument/hover", position.clone());
        assert!(
            hover["result"]["contents"]["value"]
                .as_str()
                .is_some_and(|value| value.contains("exported(arg: U32)")),
            "hover on a qualified imported symbol should show its declaration: {hover}"
        );
        let definition = client.request("textDocument/definition", position);
        assert_eq!(definition["result"]["uri"], dependency_uri);
        assert_eq!(definition["result"]["range"]["start"]["line"], 0);
        let completion = client.request(
            "textDocument/completion",
            json!({
                "textDocument":{"uri":main_uri},
                "position":{"line":3,"character":9}
            }),
        );
        assert!(
            completion["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["label"] == "exported")),
            "completion after an import alias should offer module declarations: {completion}"
        );
        let alias_completion = client.request(
            "textDocument/completion",
            json!({
                "textDocument":{"uri":main_uri},
                "position":{"line":3,"character":4}
            }),
        );
        assert!(
            alias_completion["result"]
                .as_array()
                .is_some_and(|items| { items.iter().any(|item| item["label"] == "Dep") }),
            "completion should suggest imported module aliases in code context: {alias_completion}"
        );
        client.finish();
    }
    #[test]
    fn imported_symbol_identity_survives_declaration_reordering() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path();
        let module_path = workspace.join("dep.bend");
        let importer_path = workspace.join("main.bend");
        let module_source = "def value() -> U32:\n  1\n";
        let importer_source = "import ./dep.bend as Dep\ndef caller() -> U32:\n  Dep.value()\n";
        fs::write(&module_path, module_source).must_be("write module");
        fs::write(&importer_path, importer_source).must_be("write importer");
        let module_uri = Url::from_file_path(&module_path)
            .must_be("module URI")
            .to_string();
        let importer_uri = Url::from_file_path(&importer_path)
            .must_be("importer URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&workspace.join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(workspace);
        for (uri, source) in [
            (&module_uri, module_source),
            (&importer_uri, importer_source),
        ] {
            client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":uri,"languageId":"bend","version":1,"text":source
                }}),
            );
        }
        client.request(
            "textDocument/definition",
            json!({"textDocument":{"uri":importer_uri},"position":{"line":2,"character":7}}),
        );
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":module_uri,"version":2},"contentChanges":[{
                "text":"def unrelated() -> U32:\n  0\ndef value() -> U32:\n  1\n"
            }]}),
        );
        let references = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":module_uri},"position":{"line":2,"character":5},
                "context":{"includeDeclaration":false}
            }),
        );
        assert_eq!(
            references["result"],
            json!([{"uri":importer_uri,"range":{
                "start":{"line":2,"character":6},"end":{"line":2,"character":11}
            }}]),
            "imported identity must follow value, not its previous local SymbolId"
        );
        let unrelated = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":module_uri},"position":{"line":0,"character":5},
                "context":{"includeDeclaration":false}
            }),
        );
        assert_eq!(
            unrelated["result"],
            json!([]),
            "new declaration must not inherit old symbol's workspace occurrences"
        );
        client.finish();
    }

    #[test]
    fn references_and_rename_follow_imports_without_touching_text_or_comments() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let main_source = "import ./dep.bend as Dep\ndef main() -> U32:\n  Dep.value()\n# Dep.value comment\ndef note() -> String:\n  \"Dep.value\"\ndef shadow(Dep):\n  Dep.value()\n";
        let dependency_source = "def value() -> U32:\n  1\ndef local() -> U32:\n  value()\n";
        fs::write(&main_path, main_source).must_be("write root source");
        fs::write(&dependency_path, dependency_source).must_be("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        for (uri, source) in [
            (main_uri.as_str(), main_source),
            (dependency_uri.as_str(), dependency_source),
        ] {
            client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":uri,
                    "languageId":"bend",
                    "version":1,
                    "text":source
                }}),
            );
        }
        wait_for_clean_diagnostics(&mut client, &main_uri);
        wait_for_clean_diagnostics(&mut client, &dependency_uri);
        let references = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":dependency_uri},
                "position":{"line":0,"character":5},
                "context":{"includeDeclaration":true}
            }),
        );
        assert!(
            references["result"]
                .as_array()
                .is_some_and(|locations| locations
                    .iter()
                    .any(|location| location["uri"] == main_uri)),
            "references on a module declaration should include qualified cross-file uses: {references}"
        );
        let highlights = client.request(
            "textDocument/documentHighlight",
            json!({
                "textDocument":{"uri":main_uri},
                "position":{"line":2,"character":7}
            }),
        );
        assert!(
            highlights["result"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["range"]["start"]["line"] == 2 && item["range"]["start"]["character"] == 6
                })
            }),
            "document highlights should include qualified uses in the current file: {highlights}"
        );
        let rename = client.request(
            "textDocument/rename",
            json!({
                "textDocument":{"uri":dependency_uri},
                "position":{"line":0,"character":5},
                "newName":"renamed"
            }),
        );
        assert!(
            rename["result"]["changes"][&dependency_uri]
                .as_array()
                .is_some_and(|edits| edits.len() >= 2),
            "rename should update the declaration and local use: {rename}"
        );
        assert_eq!(
            rename["result"]["changes"][&main_uri]
                .as_array()
                .map(Vec::len),
            Some(1),
            "rename should update only qualified code, not comments or strings: {rename}"
        );
        client.finish();
    }
    #[test]
    fn references_and_rename_respect_parameter_shadowing() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def target() -> U32:\n  0\ndef wrapper(target):\n  target()\ndef main: U32\n  target()\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );

        let references = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":4},
                "context":{"includeDeclaration":true}
            }),
        );
        let locations = references["result"].as_array().must_be("local references");
        assert_eq!(
            locations.len(),
            2,
            "only parameter declaration and use: {references}"
        );
        assert!(locations.iter().all(|location| {
            [2, 3].contains(
                &location["range"]["start"]["line"]
                    .as_u64()
                    .must_be("LSP reference line"),
            )
        }));

        let rename = client.request(
            "textDocument/rename",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":3,"character":4},
                "newName":"callback"
            }),
        );
        let edits = rename["result"]["changes"][&uri]
            .as_array()
            .must_be("local rename edits");
        assert_eq!(edits.len(), 2, "rename only the local binding: {rename}");
        assert!(edits.iter().all(|edit| {
            [2, 3].contains(
                &edit["range"]["start"]["line"]
                    .as_u64()
                    .must_be("LSP rename line"),
            )
        }));
        client.finish();
    }
    #[test]
    fn definition_resolves_imported_constructors_in_patterns_and_expressions() {
        let temp = tempdir().must_be("temporary workspace");
        let main_path = temp.path().join("main.bend");
        let ast_path = temp.path().join("ast.bend");
        let source = "import ./ast.bend as Ast\ndef shift_at(term: Ast.SyntaxTerm) -> Ast.SyntaxTerm:\n  match term:\n    case Ast.TermVar{key}:\n      Ast.TermVar{key}\n";
        fs::write(&main_path, source).must_be("write root source");
        fs::write(
            &ast_path,
            "type SyntaxTerm is Data:\n  TermVar{key: String}\n",
        )
        .must_be("write constructor declaration");
        let uri = Url::from_file_path(&main_path)
            .must_be("root URI")
            .to_string();
        let ast_uri = Url::from_file_path(&ast_path)
            .must_be("AST URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,"languageId":"bend","version":1,"text":source
            }}),
        );
        for (line, character) in [(3, 15), (4, 12)] {
            let definition = client.request(
                "textDocument/definition",
                json!({
                    "textDocument":{"uri":uri},
                    "position":{"line":line,"character":character}
                }),
            );
            assert_eq!(
                definition["result"],
                json!({
                    "uri":ast_uri,
                    "range":{
                        "start":{"line":1,"character":2},
                        "end":{"line":1,"character":9}
                    }
                }),
                "constructor use at {line}:{character} must navigate to its declaration"
            );
        }
        client.finish();
    }

    #[test]
    fn definition_distinguishes_module_alias_from_type_and_constructor_members() {
        let temp = tempdir().must_be("temporary workspace");
        let main_path = temp.path().join("main.bend");
        let ast_path = temp.path().join("ast.bend");
        let source = "import ./ast.bend as Ast\ndef shift_at(term: Ast.SyntaxTerm) -> Ast.SyntaxTerm:\n  match term:\n    case Ast.TermVar{key}:\n      Ast.TermVar{key}\n";
        fs::write(&main_path, source).must_be("write root source");
        fs::write(
            &ast_path,
            "type SyntaxTerm is Data:\n  TermVar{key: String}\n",
        )
        .must_be("write AST declarations");
        let uri = Url::from_file_path(&main_path)
            .must_be("root URI")
            .to_string();
        let ast_uri = Url::from_file_path(&ast_path)
            .must_be("AST URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,"languageId":"bend","version":1,"text":source
            }}),
        );
        for (line, character) in [(1, 20), (3, 10), (4, 7)] {
            let definition = client.request(
                "textDocument/definition",
                json!({
                    "textDocument":{"uri":uri},
                    "position":{"line":line,"character":character}
                }),
            );
            assert_eq!(
                definition["result"],
                json!({
                    "uri":ast_uri,
                    "range":{
                        "start":{"line":0,"character":0},
                        "end":{"line":0,"character":0}
                    }
                }),
                "cursor on a module alias at {line}:{character} must target the module"
            );
        }
        let type_definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":1,"character":25}
            }),
        );
        assert_eq!(
            type_definition["result"],
            json!({
                "uri":ast_uri,
                "range":{
                    "start":{"line":0,"character":5},
                    "end":{"line":0,"character":15}
                }
            }),
            "cursor on the member must still target the type declaration"
        );
        client.finish();
    }

    #[test]
    fn type_navigation_resolves_qualified_imported_types() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let types_path = workspace.join("types.bend");
        let source = "import ./types.bend as Types\ndef consume(shape: Types.Shape) -> U32:\n  1\n";
        fs::write(&main_path, source).must_be("write source");
        fs::write(&types_path, "type Shape is Data:\n  Circle{}\n").must_be("write types");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let types_uri = Url::from_file_path(&types_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let definition = client.request(
            "textDocument/typeDefinition",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":1,"character":28}
            }),
        );
        assert_eq!(
            definition["result"]["uri"], types_uri,
            "qualified type navigation should target the imported module: {definition}"
        );
        assert_eq!(definition["result"]["range"]["start"]["line"], 0);
        client.finish();
    }
    #[test]
    fn newer_document_version_cancels_obsolete_compiler_process() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let slow_source = "# slow\n\ndef main() -> U32:\n  1\n";
        let current_source = "def main() -> U32:\n  2\n";
        fs::write(&main_path, slow_source).must_be("write slow source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let pid_path = temp.path().join("slow-compiler.pid");
        let custom_compiler = temp.path().join("slow-bend");
        fs::write(
            &custom_compiler,
            format!(
                "#!/bin/sh\nif grep -q '^# slow$' \"$1\"; then\n  echo $$ > '{}'\n  exec sleep 30\nfi\nexit 0\n",
                pid_path.display()
            ),
        ).must_be("write slow compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .must_be("make slow compiler executable");
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":custom_compiler,
                "compilerArguments":[]
            }}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":slow_source
            }}),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fs::read_to_string(&pid_path)
            .must_be("slow compiler should have started")
            .trim()
            .to_owned();
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":uri,"version":2},
                "contentChanges":[{"text":current_source}]
            }),
        );
        assert!(diagnostics_for(
            &mut client,
            &uri,
            true,
            Duration::from_secs(5)
        ));
        let still_running = process_is_running(&pid);
        if still_running {
            let _ = Command::new("/bin/kill")
                .arg(&pid)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        assert!(
            !still_running,
            "superseded compiler subprocess {pid} should be killed when its version is obsolete"
        );
        client.finish();
    }
    struct BlockedCompiler {
        compiler_dir: PathBuf,
        started: Receiver<String>,
        completion: Receiver<String>,
        release_request: PathBuf,
    }

    impl BlockedCompiler {
        fn new(temp_dir: &Path) -> Self {
            let started_fifo = temp_dir.join("compiler-started");
            let blocked_fifo = temp_dir.join("compiler-blocked");
            let release_request = temp_dir.join("compiler-release");
            let completion_signal = temp_dir.join("compiler-released");
            for fifo in [
                &started_fifo,
                &blocked_fifo,
                &release_request,
                &completion_signal,
            ] {
                create_fifo(fifo);
            }
            let started = read_fifo_line(started_fifo.clone());
            let completion = read_fifo_line(completion_signal.clone());
            let compiler_dir = install_blocked_diagnostics_compiler(
                &temp_dir.join("bin"),
                &started_fifo,
                &blocked_fifo,
                &release_request,
                &completion_signal,
            );
            Self {
                compiler_dir,
                started,
                completion,
                release_request,
            }
        }

        fn wait_until_started(&self, reason: &str) {
            let compiler_pid = self
                .started
                .recv_timeout(Duration::from_secs(5))
                .must_be(reason);
            assert!(!compiler_pid.is_empty());
        }

        fn release(&self) {
            write_fifo_line(&self.release_request, "release\n");
            assert_eq!(
                self.completion
                    .recv_timeout(Duration::from_secs(5))
                    .must_be("release observer must finish"),
                "released"
            );
        }
    }

    #[test]
    fn blocked_superseded_root_diagnostics_do_not_publish_after_v2() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        fs::write(&main_path, "# BLOCKED_V1\nBAD\n").must_be("write v1 source");

        let blocked_compiler = BlockedCompiler::new(temp.path());
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&blocked_compiler.compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":"# BLOCKED_V1\nBAD\n"
            }}),
        );
        blocked_compiler.wait_until_started("v1 compiler must reach its barrier");

        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":uri,"version":2},
                "contentChanges":[{"text":"def main() -> U32:\n  2\n"}]
            }),
        );
        assert!(
            client
                .receive_matching(Duration::from_secs(5), |message| {
                    message["method"] == "textDocument/publishDiagnostics"
                        && message["params"]["uri"] == uri
                        && message["params"]["version"] == 2
                        && message["params"]["diagnostics"]
                            .as_array()
                            .is_some_and(Vec::is_empty)
                })
                .is_some(),
            "v2 root diagnostics must complete before releasing v1"
        );

        blocked_compiler.release();
        let _ = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            client
                .receive_matching(Duration::ZERO, |message| {
                    message["method"] == "textDocument/publishDiagnostics"
                        && message["params"]["uri"] == uri
                        && message["params"]["version"] == 1
                })
                .is_none(),
            "superseded root diagnostics for v1 must not publish after v2"
        );
        client.finish();
    }

    #[test]
    fn blocked_superseded_import_diagnostics_do_not_restore_stale_errors() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let source_v1 = "import ./dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        let source_v2 = "# BLOCKED_V1\nimport ./dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        let source_v3 = "def main() -> U32:\n  2\n";
        fs::write(&main_path, source_v1).must_be("write initial root source");
        fs::write(&dependency_path, "BAD\n").must_be("write broken dependency");

        let blocked_compiler = BlockedCompiler::new(temp.path());
        let main_uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&blocked_compiler.compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":main_uri,
                "languageId":"bend",
                "version":1,
                "text":source_v1
            }}),
        );
        assert!(
            client
                .receive_matching(Duration::from_secs(5), |message| {
                    message["method"] == "textDocument/publishDiagnostics"
                        && message["params"]["uri"] == dependency_uri
                        && message["params"]["diagnostics"]
                            .as_array()
                            .is_some_and(|diagnostics| !diagnostics.is_empty())
                })
                .is_some(),
            "initial imported diagnostic must be present before the race"
        );

        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":main_uri,"version":2},
                "contentChanges":[{"text":source_v2}]
            }),
        );
        blocked_compiler.wait_until_started("v2 compiler must reach its barrier");
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":main_uri,"version":3},
                "contentChanges":[{"text":source_v3}]
            }),
        );
        assert!(
            client
                .receive_matching(Duration::from_secs(5), |message| {
                    message["method"] == "textDocument/publishDiagnostics"
                        && message["params"]["uri"] == main_uri
                        && message["params"]["version"] == 3
                        && message["params"]["diagnostics"]
                            .as_array()
                            .is_some_and(Vec::is_empty)
                })
                .is_some(),
            "v3 root diagnostics must complete before releasing v2"
        );
        assert!(
            client
                .receive_matching(Duration::from_secs(5), |message| {
                    message["method"] == "textDocument/publishDiagnostics"
                        && message["params"]["uri"] == dependency_uri
                        && message["params"]["diagnostics"]
                            .as_array()
                            .is_some_and(Vec::is_empty)
                })
                .is_some(),
            "v3 must clear imported diagnostics from the prior committed result"
        );

        blocked_compiler.release();
        let _ = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":main_uri}}),
        );
        assert!(
            client
                .receive_matching(Duration::ZERO, |message| {
                    message["method"] == "textDocument/publishDiagnostics"
                        && message["params"]["uri"] == dependency_uri
                        && message["params"]["diagnostics"]
                            .as_array()
                            .is_some_and(|diagnostics| !diagnostics.is_empty())
                })
                .is_none(),
            "superseded imported diagnostics must not restore the v2 error after v3"
        );
        client.finish();
    }

    #[test]
    fn closing_document_cancels_running_compiler_process() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let slow_source = "# slow\n\ndef main() -> U32:\n  1\n";
        fs::write(&main_path, slow_source).must_be("write slow source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let pid_path = temp.path().join("slow-compiler.pid");
        let custom_compiler = temp.path().join("slow-bend");
        fs::write(
            &custom_compiler,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
                pid_path.display()
            ),
        )
        .must_be("write slow compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .must_be("make slow compiler executable");
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":custom_compiler,
                "compilerArguments":[]
            }}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":slow_source
            }}),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fs::read_to_string(&pid_path)
            .must_be("slow compiler should have started")
            .trim()
            .to_owned();
        client.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
        assert!(
            diagnostics_for(&mut client, &uri, true, Duration::from_secs(5)),
            "closing a document should immediately clear its diagnostics"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut still_running = true;
        while still_running && Instant::now() < deadline {
            still_running = process_is_running(&pid);
            if still_running {
                thread::sleep(Duration::from_millis(10));
            }
        }
        if still_running {
            let _ = Command::new("/bin/kill")
                .arg(&pid)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        assert!(
            !still_running,
            "closing a document must cancel its compiler child {pid}"
        );
        client.finish();
    }

    #[test]
    fn compiler_process_concurrency_is_bounded() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        let active_dir = temp.path().join("active");
        fs::create_dir_all(&workspace).must_be("create workspace");
        fs::create_dir_all(&active_dir).must_be("create compiler activity directory");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let custom_compiler = temp.path().join("counting-bend");
        fs::write(
            &custom_compiler,
            format!(
                "#!/bin/sh\ntouch '{}/pid-'$$\nactive=$(find '{}' -type f -name 'pid-*' | wc -l)\nif [ \"$active\" -gt 4 ]; then touch '{}/overflow'; fi\nsleep 0.5\nrm -f '{}/pid-'$$\nexit 0\n",
                active_dir.display(),
                active_dir.display(),
                active_dir.display(),
                active_dir.display()
            ),
        ).must_be("write counting compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .must_be("make counting compiler executable");
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":custom_compiler,
                "compilerArguments":[]
            }}}),
        );
        let source = "def main() -> U32:\n  1\n";
        let uris: Vec<String> = (0..8)
            .map(|index| {
                let path = workspace.join(format!("file-{index}.bend"));
                fs::write(&path, source).must_be("write workspace source");
                Url::from_file_path(path)
                    .must_be("valid test fixture value")
                    .to_string()
            })
            .collect();
        for uri in &uris {
            client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":uri,
                    "languageId":"bend",
                    "version":1,
                    "text":source
                }}),
            );
        }
        for uri in &uris {
            assert!(
                diagnostics_for(&mut client, uri, true, Duration::from_secs(10)),
                "every queued document should be checked: {uri}"
            );
        }
        assert!(
            !active_dir.join("overflow").exists(),
            "the compiler semaphore must keep simultaneous Bend processes at or below four"
        );
        client.finish();
    }
    #[test]
    fn sigterm_without_shutdown_cancels_active_compiler_and_flushes_trace() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main() -> U32:\n  1\n";
        fs::write(&main_path, source).must_be("write source");
        let uri = Url::from_file_path(&main_path)
            .must_be("source URI")
            .to_string();
        let started_fifo = temp.path().join("compiler-started");
        let release_fifo = temp.path().join("compiler-release");
        create_fifo(&started_fifo);
        create_fifo(&release_fifo);
        let started = read_fifo_line(started_fifo.clone());
        let compiler = temp.path().join("blocked-bend");
        fs::write(
            &compiler,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > {}\nIFS= read -r _ < {}\n",
                shell_quote(&started_fifo),
                shell_quote(&release_fifo),
            ),
        )
        .must_be("write blocked compiler");
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .must_be("make blocked compiler executable");

        let trace_path = temp.path().join("active-sigterm.json");
        let stderr_path = temp.path().join("server-stderr.txt");
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
        command
            .env("BEND2_LSP_TRACE", &trace_path)
            .stderr(Stdio::from(
                fs::File::create(&stderr_path).must_be("capture server stderr"),
            ));
        let mut client = LspClient::spawn(command);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":compiler,
                "compilerArguments":[]
            }}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let pid = started
            .recv_timeout(Duration::from_secs(5))
            .must_be("compiler must start before SIGTERM");
        client.sigterm();
        let status = client
            .wait_timeout(Duration::from_secs(2))
            .must_be("SIGTERM must terminate with stdin still open");
        assert!(status.success(), "SIGTERM is graceful shutdown: {status}");
        let still_running = process_is_running(&pid);
        if still_running {
            let _ = Command::new("/bin/kill").arg(&pid).status();
        }
        assert!(!still_running, "SIGTERM must reap compiler child {pid}");
        let stderr = fs::read_to_string(&stderr_path).must_be("read captured server stderr");
        assert!(!stderr.contains("panicked"), "server panicked: {stderr}");
        let trace = fs::read_to_string(&trace_path).must_be("read finalized trace");
        let events: Vec<Value> = serde_json::from_str(&trace).must_be("parse complete trace JSON");
        assert!(
            events.iter().any(|event| event["name"] == "compiler.child"),
            "trace must retain the active compiler span"
        );
    }

    #[test]
    fn exit_without_shutdown_cancels_compiler_child_and_keeps_stdin_open() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let slow_source = "# slow\n\ndef main() -> U32:\n  1\n";
        fs::write(&main_path, slow_source).must_be("write slow source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let pid_path = temp.path().join("slow-compiler.pid");
        let custom_compiler = temp.path().join("slow-bend");
        fs::write(
            &custom_compiler,
            format!(
                "#!/bin/sh\nif grep -q '^# slow$' \"$1\"; then\n  echo $$ > '{}'\n  exec sleep 30\nfi\nexit 0\n",
                pid_path.display()
            ),
        ).must_be("write slow compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .must_be("make slow compiler executable");
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":custom_compiler,
                "compilerArguments":[]
            }}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":slow_source
            }}),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        let pid = fs::read_to_string(&pid_path)
            .must_be("slow compiler should have started")
            .trim()
            .to_owned();
        client.notify("exit", Value::Null);
        let deadline = Instant::now() + Duration::from_secs(2);
        while client.try_wait().is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            client.try_wait().is_some(),
            "exit without shutdown must terminate while stdin remains open"
        );
        let still_running = Command::new("/bin/kill")
            .arg("-0")
            .arg(&pid)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if still_running {
            let _ = Command::new("/bin/kill")
                .arg(&pid)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        assert!(
            !still_running,
            "exit must cancel the running compiler child {pid}"
        );
    }
    fn assert_call_hierarchy(client: &mut LspClient, uri: &str) {
        let root = client.request(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":6,"character":5}
            }),
        );
        assert_eq!(root["result"][0]["name"], "root", "{root}");
        let outgoing = client.request(
            "callHierarchy/outgoingCalls",
            json!({"item":root["result"][0]}),
        );
        assert!(
            outgoing["result"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["to"]["name"] == "leaf")),
            "outgoing calls should find the invoked declaration: {outgoing}"
        );
        assert!(
            outgoing["result"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["to"]["name"] == "external")),
            "outgoing calls should resolve imported declarations: {outgoing}"
        );
        let leaf = client.request(
            "textDocument/prepareCallHierarchy",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":4,"character":5}
            }),
        );
        let incoming = client.request(
            "callHierarchy/incomingCalls",
            json!({"item":leaf["result"][0]}),
        );
        assert!(
            incoming["result"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["from"]["name"] == "root")),
            "incoming calls should identify the caller declaration: {incoming}"
        );
        let external = outgoing["result"]
            .as_array()
            .must_be("valid test fixture value")
            .iter()
            .find(|call| call["to"]["name"] == "external")
            .must_be("outgoing calls should include the imported function")["to"]
            .clone();
        let external_incoming =
            client.request("callHierarchy/incomingCalls", json!({"item":external}));
        assert!(
            external_incoming["result"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["from"]["name"] == "root")),
            "incoming calls should resolve imported declarations: {external_incoming}"
        );
    }

    fn assert_type_hierarchy(client: &mut LspClient, uri: &str) {
        let shape = client.request(
            "textDocument/prepareTypeHierarchy",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":8,"character":14}
            }),
        );
        assert_eq!(shape["result"][0]["name"], "Shape", "{shape}");
        let subtypes = client.request("typeHierarchy/subtypes", json!({"item":shape["result"][0]}));
        assert!(
            subtypes["result"].as_array().is_some_and(|items| {
                items.iter().any(|item| item["name"] == "Circle")
                    && items.iter().any(|item| item["name"] == "Square")
            }),
            "algebraic constructors should appear under their declaring type: {subtypes}"
        );
        let circle = client.request(
            "textDocument/prepareTypeHierarchy",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":2,"character":4}
            }),
        );
        let supertypes = client.request(
            "typeHierarchy/supertypes",
            json!({"item":circle["result"][0]}),
        );
        assert!(
            supertypes["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == "Shape")),
            "a constructor should link back to its declaring type: {supertypes}"
        );
    }

    #[test]
    fn call_and_algebraic_type_hierarchies_follow_declarations_and_calls() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let dependency_source = "def external(x: U32) -> U32:\n  x\n";
        fs::write(&dependency_path, dependency_source).must_be("write dependency");
        let source = "import ./dep.bend as Dep\ntype Shape is Data:\n  Circle{}\n  Square{}\ndef leaf(x: U32) -> U32:\n  x\ndef root(x: U32) -> U32:\n  leaf(x) + Dep.external(x)\ndef shape(x: Shape) -> U32:\n  x\n";
        fs::write(&main_path, source).must_be("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        let initialized = client.request(
            "initialize",
            json!({
                "processId":null,
                "rootUri":Url::from_directory_path(&workspace).must_be("workspace URI"),
                "workspaceFolders":[{"uri":Url::from_directory_path(&workspace).must_be("workspace folder URI"),"name":"test"}],
                "capabilities":{"textDocument":{"typeHierarchy":{"dynamicRegistration":true}}}
            }),
        );
        assert!(
            initialized["result"]["capabilities"]["callHierarchyProvider"] == true,
            "call hierarchy must be advertised: {initialized}"
        );
        client.notify("initialized", json!({}));
        let registration = client
            .receive_matching(Duration::from_secs(5), |message| {
                message["method"] == "client/registerCapability"
            })
            .must_be("server should dynamically register type hierarchy");
        assert_eq!(
            registration["params"]["registrations"][0]["method"],
            "textDocument/prepareTypeHierarchy"
        );
        assert_eq!(
            registration["params"]["registrations"][0]["registerOptions"]["documentSelector"][0]["language"],
            "bend"
        );
        client.send(&json!({
            "jsonrpc":"2.0",
            "id":registration["id"],
            "result":null
        }));
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        assert_call_hierarchy(&mut client, &uri);
        assert_type_hierarchy(&mut client, &uri);
        client.finish();
    }
    #[test]
    fn compiler_cache_reuses_graphs_and_invalidates_exact_inputs() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let compiler = temp.path().join("counting-bend");
        let calls = temp.path().join("calls");
        fs::write(
            &compiler,
            format!(
                "#!/bin/sh\nprintf 'called\\n' >> '{}'\nexit 0\n",
                calls.display()
            ),
        )
        .must_be("write counting compiler");
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .must_be("make counting compiler executable");

        let dependency_path = workspace.join("dep.bend");
        let unrelated_path = workspace.join("unrelated.bend");
        let main_path = workspace.join("main.bend");
        let second_path = workspace.join("second.bend");
        let dependency_uri = Url::from_file_path(&dependency_path)
            .must_be("dependency URI")
            .to_string();
        let unrelated_uri = Url::from_file_path(&unrelated_path)
            .must_be("unrelated URI")
            .to_string();
        let uri = Url::from_file_path(&main_path)
            .must_be("main URI")
            .to_string();
        let second_uri = Url::from_file_path(&second_path)
            .must_be("second root URI")
            .to_string();
        fs::write(&dependency_path, "def value: U32\n  1\n").must_be("write dependency");
        fs::write(&unrelated_path, "def isolated: U32\n  1\n").must_be("write unrelated file");
        let source = "import ./dep.bend as Dep\ndef main: U32\n  Dep.value\n";
        let second_source = "import ./dep.bend as Dep\ndef second: U32\n  Dep.value\n";
        fs::write(&main_path, source).must_be("write first root");
        fs::write(&second_path, second_source).must_be("write second root");

        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        configure_counting_compiler(&mut client, &compiler);
        open_clean_document(&mut client, &uri, source);
        assert_compiler_runs(&calls, 1, "the first graph must spawn Bend");
        open_clean_document(&mut client, &second_uri, second_source);
        assert_compiler_runs(&calls, 2, "a distinct root has its own cached result");

        change_clean_document(&mut client, &uri, 2, source);
        assert_compiler_runs(
            &calls,
            2,
            "identical text at a newer revision must reuse the result",
        );

        fs::write(&unrelated_path, "def isolated: U32\n  2\n").must_be("change unrelated file");
        notify_watched_file_change(&mut client, &unrelated_uri);
        change_clean_document(&mut client, &uri, 3, source);
        change_clean_document(&mut client, &second_uri, 2, second_source);
        assert_compiler_runs(
            &calls,
            2,
            "an unrelated file update must not invalidate either root",
        );

        fs::write(&dependency_path, "def value: U32\n  2\n").must_be("change dependency");
        notify_watched_file_change(&mut client, &dependency_uri);
        wait_for_clean_diagnostics(&mut client, &uri);
        wait_for_clean_diagnostics(&mut client, &second_uri);
        assert_compiler_runs(
            &calls,
            4,
            "the shared dependency must invalidate exactly its two root graphs",
        );

        let mut compiler_source =
            fs::read_to_string(&compiler).must_be("read compiler before stamp change");
        compiler_source.push_str("# compiler stamp changed\n");
        fs::write(&compiler, compiler_source).must_be("change compiler stamp");
        configure_counting_compiler(&mut client, &compiler);
        wait_for_clean_diagnostics(&mut client, &uri);
        wait_for_clean_diagnostics(&mut client, &second_uri);
        assert_compiler_runs(
            &calls,
            6,
            "changing the compiler stamp must invalidate both cached root graphs",
        );
        client.finish();
    }
    const BEND_CACHE_FILE_COUNT: usize = 102;

    struct BendCacheFiles {
        root: PathBuf,
        second: PathBuf,
        dependency: PathBuf,
        unrelated: PathBuf,
    }

    fn create_bend_cache_files(workspace: &Path) -> BendCacheFiles {
        use std::fmt::Write as _;

        fs::create_dir_all(workspace).must_be("create workspace");
        for index in 0..BEND_CACHE_FILE_COUNT {
            let mut source = String::new();
            if index < 2 {
                for target in 2..100 {
                    let alias = if target == 99 {
                        "Common".to_owned()
                    } else {
                        format!("File{target:03}")
                    };
                    writeln!(source, "import ./f{target:03}.bend as {alias}")
                        .must_be("generate root imports");
                }
                let private = 100 + index;
                writeln!(source, "import ./f{private:03}.bend as Private")
                    .must_be("generate private import");
                source.push_str(if index == 0 {
                    "def main: U32\n  1\n"
                } else {
                    "def second: U32\n  Common.value\n"
                });
            } else if index == 99 {
                source.push_str("def value: U32\n  1\n");
            } else if index >= 100 {
                writeln!(source, "def private_{index:03}: U32\n  1")
                    .must_be("generate private module");
            } else {
                writeln!(source, "def worker_{index:03}: U32\n  1")
                    .must_be("generate workspace source");
            }
            fs::write(workspace.join(format!("f{index:03}.bend")), source)
                .must_be("write generated workspace source");
        }

        let unrelated = workspace.join("unrelated.bend");
        fs::write(&unrelated, "def isolated: U32\n  1\n").must_be("write unrelated file");
        BendCacheFiles {
            root: workspace.join("f000.bend"),
            second: workspace.join("f001.bend"),
            dependency: workspace.join("f099.bend"),
            unrelated,
        }
    }

    fn create_bend_counting_wrappers(temp_dir: &Path, calls: &Path) -> (PathBuf, PathBuf) {
        let bend = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("bend"))
            .find(|path| path.is_file())
            .must_be("locate installed Bend CLI");
        let version = Command::new(&bend)
            .arg("version")
            .output()
            .must_be("query installed Bend CLI version");
        assert!(version.status.success(), "Bend version command failed");
        eprintln!(
            "BEND_CACHE_MEASURE bend_version={}",
            String::from_utf8_lossy(&version.stdout).trim()
        );

        let script = format!(
            "#!/bin/sh\nprintf 'called\\n' >> {}\n{} \"$@\"\n",
            shell_quote(calls),
            shell_quote(&bend)
        );
        let primary_wrapper = temp_dir.join("bend-wrapper-a");
        let alternate_wrapper = temp_dir.join("bend-wrapper-b");
        for wrapper in [&primary_wrapper, &alternate_wrapper] {
            fs::write(wrapper, &script).must_be("write Bend counting wrapper");
            fs::set_permissions(wrapper, fs::Permissions::from_mode(0o755))
                .must_be("make Bend wrapper executable");
        }
        (primary_wrapper, alternate_wrapper)
    }

    fn report_bend_measurement(scenario: &str, before: usize, started: Instant, calls: &Path) {
        let total = compiler_run_count(calls);
        eprintln!(
            "BEND_CACHE_MEASURE scenario={scenario} process_delta={} process_total={total} wall_ms={:.3}",
            total - before,
            started.elapsed().as_secs_f64() * 1_000.0
        );
    }

    fn compiler_metric_field<'a>(line: &'a str, field: &str) -> &'a str {
        let prefix = format!("{field}=");
        line.split_whitespace()
            .find_map(|part| part.strip_prefix(&prefix))
            .must_be("compiler metric field")
    }

    fn wait_for_bend_diagnostics(client: &mut LspClient, uri: &str, version: i32) {
        let diagnostics = client
            .receive_matching(Duration::from_secs(60), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && message["params"]["version"] == version
            })
            .must_be("receive real Bend diagnostics");
        assert!(
            diagnostics["params"]["diagnostics"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "installed Bend reported diagnostics: {diagnostics}"
        );
    }

    struct BendCacheMeasurement {
        client: LspClient,
        calls: PathBuf,
        metrics: PathBuf,
        metrics_reported: usize,
        root_uri: String,
        second_uri: String,
        dependency_uri: String,
        unrelated_uri: String,
        root_source: String,
        second_source: String,
        dependency_path: PathBuf,
        unrelated_path: PathBuf,
        primary_compiler_wrapper: PathBuf,
        alternate_compiler_wrapper: PathBuf,
        _temp: tempfile::TempDir,
    }

    impl BendCacheMeasurement {
        fn new() -> Self {
            let temp = tempdir().must_be("temporary workspace");
            let workspace = temp.path().join("workspace");
            let files = create_bend_cache_files(&workspace);
            let calls = temp.path().join("bend-calls");
            fs::write(&calls, "").must_be("create compiler call log");
            let metrics = temp.path().join("compiler-metrics");
            fs::write(&metrics, "").must_be("create compiler metrics log");
            let (primary_compiler_wrapper, alternate_compiler_wrapper) =
                create_bend_counting_wrappers(temp.path(), &calls);
            let root_uri = Url::from_file_path(&files.root)
                .must_be("root URI")
                .to_string();
            let second_uri = Url::from_file_path(&files.second)
                .must_be("second root URI")
                .to_string();
            let dependency_uri = Url::from_file_path(&files.dependency)
                .must_be("dependency URI")
                .to_string();
            let unrelated_uri = Url::from_file_path(&files.unrelated)
                .must_be("unrelated URI")
                .to_string();
            let root_source = fs::read_to_string(&files.root).must_be("read root source");
            let second_source =
                fs::read_to_string(&files.second).must_be("read second root source");
            let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
            let mut client = spawn_with_compiler_metrics(&compiler_dir, &metrics);
            client.initialize(&workspace);
            configure_counting_compiler(&mut client, &primary_compiler_wrapper);

            Self {
                client,
                calls,
                metrics,
                metrics_reported: 0,
                root_uri,
                second_uri,
                dependency_uri,
                unrelated_uri,
                root_source,
                second_source,
                dependency_path: files.dependency,
                unrelated_path: files.unrelated,
                primary_compiler_wrapper,
                alternate_compiler_wrapper,
                _temp: temp,
            }
        }

        fn assert_runs(&self, expected: usize, scenario: &str) {
            assert_compiler_runs(&self.calls, expected, scenario);
        }

        fn report(
            &mut self,
            scenario: &str,
            before: usize,
            started: Instant,
            expected_checks: usize,
            expected_cache_hit: Option<bool>,
            expected_staged_files: Option<usize>,
        ) {
            report_bend_measurement(scenario, before, started, &self.calls);
            let lines: Vec<_> = fs::read_to_string(&self.metrics)
                .must_be("read compiler metrics")
                .lines()
                .map(str::to_owned)
                .collect();
            let new_lines = lines
                .get(self.metrics_reported..)
                .must_be("compiler metrics must not be truncated");
            assert_eq!(
                new_lines.len(),
                expected_checks,
                "unexpected compiler metric rows for {scenario}"
            );
            for line in new_lines {
                assert!(line.starts_with("BEND2_COMPILER_METRIC "));
                if let Some(expected) = expected_cache_hit {
                    assert_eq!(
                        compiler_metric_field(line, "cache"),
                        if expected { "hit" } else { "miss" },
                        "cache state for {scenario}: {line}"
                    );
                }
                let staged_files: u128 = compiler_metric_field(line, "staged_files")
                    .parse()
                    .must_be("parse staged file count");
                if let Some(expected) = expected_staged_files {
                    assert_eq!(staged_files, expected as u128, "{scenario}: {line}");
                }
                let staged_bytes: u128 = compiler_metric_field(line, "staged_bytes")
                    .parse()
                    .must_be("parse staged byte count");
                let staging_ns: u128 = compiler_metric_field(line, "staging_ns")
                    .parse()
                    .must_be("parse staging duration");
                let child_ns: u128 = compiler_metric_field(line, "child_ns")
                    .parse()
                    .must_be("parse child duration");
                let total_ns: u128 = compiler_metric_field(line, "total_ns")
                    .parse()
                    .must_be("parse total diagnostics duration");
                assert!(total_ns > 0, "total diagnostics duration: {line}");
                if staged_files == 0 {
                    assert_eq!((staged_bytes, staging_ns, child_ns), (0, 0, 0), "{line}");
                } else {
                    assert!(staged_bytes > 0, "staged bytes: {line}");
                    assert!(staging_ns > 0, "staging duration: {line}");
                    assert!(child_ns > 0, "child duration: {line}");
                }
                eprintln!("BEND_CACHE_CHECK scenario={scenario} {line}");
            }
            self.metrics_reported = lines.len();
        }

        fn measure_initial_scenarios(&mut self) {
            let before = compiler_run_count(&self.calls);
            let started = Instant::now();
            self.client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":self.root_uri,
                    "languageId":"bend",
                    "version":1,
                    "text":self.root_source
                }}),
            );
            wait_for_bend_diagnostics(&mut self.client, &self.root_uri, 1);
            self.assert_runs(1, "first 100-file graph check");
            self.report(
                "first_root_100_file_graph",
                before,
                started,
                1,
                Some(false),
                Some(100),
            );

            let before = compiler_run_count(&self.calls);
            let started = Instant::now();
            self.client.notify(
                "textDocument/didChange",
                json!({"textDocument":{"uri":self.root_uri,"version":1},"contentChanges":[{"text":self.root_source}]}),
            );
            let _barrier = self.client.request(
                "textDocument/hover",
                json!({"textDocument":{"uri":self.root_uri},"position":{"line":0,"character":0}}),
            );
            self.assert_runs(1, "identical same-version notification");
            self.report("identical_same_revision", before, started, 0, None, None);

            let before = compiler_run_count(&self.calls);
            let started = Instant::now();
            self.client.notify(
                "textDocument/didOpen",
                json!({"textDocument":{
                    "uri":self.second_uri,
                    "languageId":"bend",
                    "version":1,
                    "text":self.second_source
                }}),
            );
            wait_for_bend_diagnostics(&mut self.client, &self.second_uri, 1);
            self.assert_runs(2, "distinct root check");
            self.report("distinct_root", before, started, 1, Some(false), Some(100));

            let before = compiler_run_count(&self.calls);
            let started = Instant::now();
            self.client.notify(
                "textDocument/didChange",
                json!({"textDocument":{"uri":self.root_uri,"version":2},"contentChanges":[{"text":self.root_source}]}),
            );
            wait_for_bend_diagnostics(&mut self.client, &self.root_uri, 2);
            self.assert_runs(2, "identical text at a newer revision");
            self.report(
                "same_text_new_revision",
                before,
                started,
                1,
                Some(true),
                Some(0),
            );
        }

        fn measure_workspace_edits(&mut self) {
            let before = compiler_run_count(&self.calls);
            let started = Instant::now();
            fs::write(&self.unrelated_path, "def isolated: U32\n  2\n")
                .must_be("change unrelated file");
            notify_watched_file_change(&mut self.client, &self.unrelated_uri);
            self.client.notify(
                "textDocument/didChange",
                json!({"textDocument":{"uri":self.root_uri,"version":3},"contentChanges":[{"text":self.root_source}]}),
            );
            self.client.notify(
                "textDocument/didChange",
                json!({"textDocument":{"uri":self.second_uri,"version":2},"contentChanges":[{"text":self.second_source}]}),
            );
            wait_for_bend_diagnostics(&mut self.client, &self.root_uri, 3);
            wait_for_bend_diagnostics(&mut self.client, &self.second_uri, 2);
            self.assert_runs(2, "unrelated file edit");
            self.report(
                "unrelated_file_edit",
                before,
                started,
                2,
                Some(true),
                Some(0),
            );

            let before = compiler_run_count(&self.calls);
            let started = Instant::now();
            fs::write(&self.dependency_path, "def value: U32\n  2\n")
                .must_be("change shared dependency");
            notify_watched_file_change(&mut self.client, &self.dependency_uri);
            wait_for_bend_diagnostics(&mut self.client, &self.root_uri, 3);
            wait_for_bend_diagnostics(&mut self.client, &self.second_uri, 2);
            self.assert_runs(4, "shared dependency edit invalidates both roots");
            self.report(
                "shared_dependency_edit",
                before,
                started,
                2,
                Some(false),
                Some(100),
            );
        }

        fn measure_compiler_changes(&mut self) {
            let before = compiler_run_count(&self.calls);
            let started = Instant::now();
            let mut wrapper = fs::OpenOptions::new()
                .append(true)
                .open(&self.primary_compiler_wrapper)
                .must_be("open compiler wrapper for stamp change");
            writeln!(wrapper, "# compiler stamp changed").must_be("change compiler stamp");
            configure_counting_compiler(&mut self.client, &self.primary_compiler_wrapper);
            wait_for_bend_diagnostics(&mut self.client, &self.root_uri, 3);
            wait_for_bend_diagnostics(&mut self.client, &self.second_uri, 2);
            self.assert_runs(6, "compiler stamp change invalidates both roots");
            self.report(
                "compiler_stamp_change",
                before,
                started,
                2,
                Some(false),
                Some(100),
            );

            let before = compiler_run_count(&self.calls);
            let started = Instant::now();
            configure_counting_compiler(&mut self.client, &self.alternate_compiler_wrapper);
            wait_for_bend_diagnostics(&mut self.client, &self.root_uri, 3);
            wait_for_bend_diagnostics(&mut self.client, &self.second_uri, 2);
            self.assert_runs(8, "compiler path change invalidates both roots");
            self.report(
                "compiler_path_change",
                before,
                started,
                2,
                Some(false),
                Some(100),
            );
        }

        fn finish(self) {
            self.client.finish();
        }
    }

    #[ignore = "requires Bend CLI on PATH; emits local timing samples"]
    #[test]
    fn measure_actual_bend_compiler_cache_fallback() {
        let mut measurement = BendCacheMeasurement::new();
        measurement.measure_initial_scenarios();
        measurement.measure_workspace_edits();
        measurement.measure_compiler_changes();
        measurement.finish();
    }

    #[test]
    fn large_staging_edits_keep_hover_requests_responsive() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let changing_path = workspace.join("changing.bend");
        let observed_path = workspace.join("observed.bend");
        let changing_initial = "def initial_change: U32\n  1\n";
        let observed_initial = "def revision_one: U32\n  1\n";
        fs::write(&changing_path, changing_initial).must_be("write changing source");
        fs::write(&observed_path, observed_initial).must_be("write observed source");
        let changing_uri = Url::from_file_path(&changing_path)
            .must_be("changing document URI")
            .to_string();
        let observed_uri = Url::from_file_path(&observed_path)
            .must_be("observed document URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        open_clean_document(&mut client, &changing_uri, changing_initial);
        open_clean_document(&mut client, &observed_uri, observed_initial);

        let latest = "def latest_committed: U32\n  2\n";
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":observed_uri,"version":2},"contentChanges":[{"text":latest}]}),
        );
        let confirmed = client.request(
            "textDocument/hover",
            json!({"textDocument":{"uri":observed_uri},"position":{"line":0,"character":5}}),
        );
        assert!(
            confirmed["result"]["contents"]["value"]
                .as_str()
                .is_some_and(|value| value.contains("latest_committed")),
            "observed document must be committed at v2 before the large edit: {confirmed}"
        );

        let large = include_str!("../benches/fixtures/analyzer_large.bend");
        let revised = format!("def large_edit: U32\n  1\n{large}\n{large}\n{large}\n{large}");
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":changing_uri,"version":2},
                "contentChanges":[{"text":revised}]
            }),
        );
        let hover = json!({
            "textDocument":{"uri":observed_uri},
            "position":{"line":0,"character":5}
        });
        let requests: Vec<(&str, Value)> = (0..32)
            .map(|_| ("textDocument/hover", hover.clone()))
            .collect();
        let responses = request_burst_results(&mut client, &requests);
        let mut latencies = responses
            .iter()
            .map(|(_, latency)| *latency)
            .collect::<Vec<_>>();
        latencies.sort_unstable();
        let p50 = latencies[(latencies.len() - 1) / 2];
        let p95 = latencies[(latencies.len() * 95).div_ceil(100) - 1];
        let max = *latencies.last().must_be("hover latency samples");
        eprintln!(
            "large-edit unrelated protocol hover latency: p50={}us p95={}us max={}us",
            p50.as_micros(),
            p95.as_micros(),
            max.as_micros()
        );
        assert!(
            p95 < Duration::from_secs(1),
            "large staging edit blocked unrelated hover requests: p95={p95:?}, max={max:?}"
        );
        for (response, _) in responses {
            assert!(
                response["result"]["contents"]["value"]
                    .as_str()
                    .is_some_and(|value| value.contains("latest_committed")),
                "concurrent reads must see the latest committed preceding revision: {response}"
            );
        }
        let diagnostics = client
            .receive_matching(Duration::from_secs(20), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == changing_uri
                    && message["params"]["version"] == 2
            })
            .must_be("diagnostics for the committed large revision");
        assert!(
            diagnostics["params"]["diagnostics"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        client.finish();
    }
    #[test]
    fn unrelated_large_open_does_not_delay_committed_document_queries() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let opening_path = workspace.join("opening.bend");
        let observed_path = workspace.join("observed.bend");
        let observed_source = "def stable_document: U32\n  1\n";
        fs::write(&observed_path, observed_source).must_be("write observed source");
        let opening_uri = Url::from_file_path(&opening_path)
            .must_be("opening document URI")
            .to_string();
        let observed_uri = Url::from_file_path(&observed_path)
            .must_be("observed document URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        open_clean_document(&mut client, &observed_uri, observed_source);

        let large = include_str!("../benches/fixtures/analyzer_large.bend");
        let opening_source = format!("{large}\n{large}\n{large}\n{large}");
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":opening_uri,
                "languageId":"bend",
                "version":1,
                "text":opening_source
            }}),
        );
        let hover = json!({
            "textDocument":{"uri":observed_uri},
            "position":{"line":0,"character":5}
        });
        let requests: Vec<(&str, Value)> = (0..32)
            .map(|_| ("textDocument/hover", hover.clone()))
            .collect();
        let responses = request_burst_results(&mut client, &requests);
        let mut latencies = responses
            .iter()
            .map(|(_, latency)| *latency)
            .collect::<Vec<_>>();
        latencies.sort_unstable();
        let p50 = latencies[(latencies.len() - 1) / 2];
        let p95 = latencies[(latencies.len() * 95).div_ceil(100) - 1];
        let max = *latencies.last().must_be("hover latency samples");
        eprintln!(
            "unrelated-open protocol hover latency: p50={}us p95={}us max={}us",
            p50.as_micros(),
            p95.as_micros(),
            max.as_micros()
        );
        assert!(
            p95 < Duration::from_secs(1),
            "unrelated large open delayed committed-document queries: p95={p95:?}, max={max:?}"
        );
        for (response, _) in responses {
            assert!(
                response["result"]["contents"]["value"]
                    .as_str()
                    .is_some_and(|value| value.contains("stable_document")),
                "query must read the already committed document during another open: {response}"
            );
        }
        let diagnostics = client
            .receive_matching(Duration::from_secs(20), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == opening_uri
                    && message["params"]["version"] == 1
            })
            .must_be("diagnostics for the large opened document");
        assert!(
            diagnostics["params"]["diagnostics"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        client.finish();
    }
    #[test]
    fn ambiguous_compiler_excerpt_falls_back_to_root_without_misattributing_import_errors() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let main_path = workspace.join("main.bend");
        let first_dependency = workspace.join("first.bend");
        let second_dependency = workspace.join("second.bend");
        let source = "import ./first.bend as First\nimport ./second.bend as Second\ndef main: U32\n  First.value\n";
        let repeated_excerpt = "def value: U32\n  1\n";
        fs::write(&main_path, source).must_be("write root source");
        fs::write(&first_dependency, repeated_excerpt).must_be("write first dependency");
        fs::write(&second_dependency, repeated_excerpt).must_be("write second dependency");
        let compiler = temp.path().join("ambiguous-bend");
        fs::write(
            &compiler,
            "#!/bin/sh\nprintf 'Error:\\nambiguous compiler error\\nLocation:\\n1>| def value: U32\\n' >&2\nexit 1\n",
        ).must_be("write compiler");
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .must_be("make compiler executable");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path)
            .must_be("valid test fixture value")
            .to_string();
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":compiler,
                "compilerArguments":[]
            }}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,
                "languageId":"bend",
                "version":1,
                "text":source
            }}),
        );
        let diagnostics = client
            .receive_matching(Duration::from_secs(5), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && message["params"]["diagnostics"]
                        .as_array()
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                item["message"] == "ambiguous compiler error"
                                    && item["range"]["start"]["line"] == 0
                                    && item["range"]["start"]["character"] == 0
                            })
                        })
            })
            .must_be("ambiguous excerpt should fall back to a root diagnostic");
        assert!(
            diagnostics["params"]["diagnostics"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| {
                    item["message"] == "ambiguous compiler error"
                        && item["range"]["end"]["line"] == 0
                        && item["range"]["end"]["character"] == 0
                })),
            "ambiguous compiler excerpts must not be assigned to an arbitrary import: {diagnostics}"
        );
        client.finish();
    }

    fn assert_reference_locations(actual: &[Value], expected: &[Value]) {
        assert_eq!(
            actual.len(),
            expected.len(),
            "unexpected reference set: {actual:?}"
        );
        for location in expected {
            assert!(
                actual.contains(location),
                "missing reference {location}: {actual:?}"
            );
        }
    }

    fn open_formatter_document(client: &mut LspClient, uri: &str, source: &str) {
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,"languageId":"bend","version":1,"text":source
            }}),
        );
        client
            .receive_matching(Duration::from_secs(10), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && message["params"]["version"] == 1
            })
            .must_be("formatter source diagnostics");
    }

    fn regression_file_uri(path: &Path) -> String {
        Url::from_file_path(path).must_be("fixture URI").to_string()
    }

    fn finish_watched_disk_read(
        client: &mut LspClient,
        mut writer: fs::File,
        path: &Path,
        source: &str,
        sentinel_uri: &str,
    ) {
        client.notify(
            "textDocument/didClose",
            json!({"textDocument":{"uri":sentinel_uri}}),
        );
        writer
            .write_all(source.as_bytes())
            .must_be("release watched snapshot");
        drop(writer);
        fs::remove_file(path).must_be("remove released watcher FIFO");
        fs::write(path, source).must_be("restore regular watched file");
        client
            .receive_matching(Duration::from_secs(5), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == sentinel_uri
                    && message["params"]["version"].is_null()
            })
            .must_be("sentinel close must complete after the watched update");
    }

    #[test]
    fn b4_on_type_indentation_recognizes_declarations_and_lexical_block_boundaries() {
        let temp = tempdir().must_be("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(&workspace);

        let cases = [
            (
                "colonless",
                "def main: U32\n  1\n",
                1,
                0,
                2,
                true,
                json!([]),
            ),
            (
                "trailing-colon",
                "def main: U32:\n  1\n",
                1,
                0,
                2,
                true,
                json!([]),
            ),
            (
                "unicode-crlf",
                "def greet(value: String) -> String # 🦀: not a delimiter\r\n\"🦀\"\r\n",
                1,
                4,
                4,
                true,
                json!([{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":0}},"newText":"    "}]),
            ),
            (
                "nested-block",
                "def choose(value):\n  match value: # 🦀\n  case None:\n    0\n",
                2,
                2,
                2,
                true,
                json!([{"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":2}},"newText":"    "}]),
            ),
            (
                "literal-and-comment-colons",
                "def main: String\n  \"# 🦀:\" # comment:\n  \"next\"\n",
                2,
                0,
                2,
                true,
                json!([]),
            ),
            (
                "tab-indentation",
                "def main: U32 # body follows\r\n1\r\n",
                1,
                0,
                8,
                false,
                json!([{"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":0}},"newText":"\t"}]),
            ),
            (
                "incomplete-literal",
                "def main: String\n  \"🦀:\n    next\n",
                2,
                0,
                2,
                true,
                json!([]),
            ),
        ];
        for (name, source, line, character, tab_size, insert_spaces, expected) in cases {
            let path = workspace.join(format!("{name}.bend"));
            fs::write(&path, source).must_be("write formatter source");
            let uri = Url::from_file_path(&path)
                .must_be("formatter source URI")
                .to_string();
            open_formatter_document(&mut client, &uri, source);
            let response = client.request(
                "textDocument/onTypeFormatting",
                json!({
                    "textDocument":{"uri":uri},
                    "position":{"line":line,"character":character},
                    "ch":"\n",
                    "options":{"tabSize":tab_size,"insertSpaces":insert_spaces}
                }),
            );
            assert_eq!(
                response["result"], expected,
                "newline formatting must preserve lexical content and infer body indentation for {name}: {response}"
            );
        }
        client.finish();
    }

    #[test]
    fn b1_navigation_respects_parameter_shadowing_and_annotation_scope() {
        let temp = tempdir().must_be("temporary workspace");
        let main_path = temp.path().join("main.bend");
        let source = "def target(value):\n  value\ndef shadow(target):\n  target(1)\ndef annotated(target: U32):\n  target\ntype Hidden:\n  One\ndef named(invisible: Hidden):\n  invisible\ndef unknown:\n  invisible\n";
        fs::write(&main_path, source).must_be("write source");
        let uri = Url::from_file_path(&main_path)
            .must_be("source URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":uri,"languageId":"bend","version":1,"text":source
            }}),
        );
        let position = json!({"textDocument":{"uri":uri},"position":{"line":3,"character":3}});
        for method in [
            "textDocument/definition",
            "textDocument/hover",
            "textDocument/typeDefinition",
        ] {
            let response = client.request(method, position.clone());
            assert_eq!(
                response["result"],
                Value::Null,
                "shadowed untyped parameter: {method}"
            );
        }
        for method in ["textDocument/hover", "textDocument/typeDefinition"] {
            let response = client.request(
                method,
                json!({"textDocument":{"uri":uri},"position":{"line":11,"character":3}}),
            );
            assert_eq!(
                response["result"],
                Value::Null,
                "an unbound name must not inherit another function's parameter annotation: {method}"
            );
        }
        let typed_hover = client.request(
            "textDocument/hover",
            json!({
                "textDocument":{"uri":uri},"position":{"line":5,"character":3}
            }),
        );
        assert_eq!(
            typed_hover["result"],
            json!({
                "contents":{"kind":"markdown","value":"```bend\ntarget: U32\n```"}
            })
        );
        let declaration = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":uri},"position":{"line":0,"character":5}
            }),
        );
        assert_eq!(
            declaration["result"],
            json!({
                "uri":uri,"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":10}}
            })
        );
        let mut rename = position;
        rename["newName"] = json!("local");
        let renamed = client.request("textDocument/rename", rename);
        assert_eq!(
            renamed["result"],
            json!({"changes":{(uri):[
                {"range":{"start":{"line":2,"character":11},"end":{"line":2,"character":17}},"newText":"local"},
                {"range":{"start":{"line":3,"character":2},"end":{"line":3,"character":8}},"newText":"local"}
            ]}})
        );
        client.finish();
    }

    #[test]
    fn b2_b3_module_cursor_resolution_rejects_shadowed_members_and_alias_rename() {
        let temp = tempdir().must_be("temporary workspace");
        let main_path = temp.path().join("main.bend");
        let dep_path = temp.path().join("dep.bend");
        let source = "import ./dep.bend as Left\ndef shadow(Left):\n  Left.shared(1)\ndef main: U32\n  Left.shared(1)\n";
        fs::write(&main_path, source).must_be("write source");
        fs::write(&dep_path, "def shared(x: U32) -> U32:\n  x\n").must_be("write dependency");
        let uri = Url::from_file_path(&main_path)
            .must_be("source URI")
            .to_string();
        let dep_uri = Url::from_file_path(&dep_path)
            .must_be("dependency URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        open_clean_document(&mut client, &uri, source);
        for method in [
            "textDocument/definition",
            "textDocument/hover",
            "textDocument/rename",
            "textDocument/references",
            "textDocument/documentHighlight",
            "textDocument/typeDefinition",
        ] {
            let response = client.request(
                method,
                json!({
                    "textDocument":{"uri":uri},"position":{"line":2,"character":8},
                    "newName":"changed","context":{"includeDeclaration":true}
                }),
            );
            assert_eq!(
                response["result"],
                Value::Null,
                "shadowed qualifier member: {method}"
            );
        }
        for (line, character) in [(0, 21), (4, 3)] {
            for method in [
                "textDocument/rename",
                "textDocument/hover",
                "textDocument/references",
                "textDocument/documentHighlight",
            ] {
                let response = client.request(
                    method,
                    json!({
                        "textDocument":{"uri":uri},"position":{"line":line,"character":character},
                        "newName":"NewLeft","context":{"includeDeclaration":true}
                    }),
                );
                assert_eq!(
                    response["result"],
                    Value::Null,
                    "unsupported module alias: {method}"
                );
            }
        }
        let alias_definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":uri},"position":{"line":4,"character":3}
            }),
        );
        assert_eq!(
            alias_definition["result"],
            json!({
                "uri":dep_uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}}
            })
        );
        let member_position =
            json!({"textDocument":{"uri":uri},"position":{"line":4,"character":8}});
        let definition = client.request("textDocument/definition", member_position.clone());
        assert_eq!(
            definition["result"],
            json!({
                "uri":dep_uri,"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":10}}
            })
        );
        let hover = client.request("textDocument/hover", member_position.clone());
        assert_eq!(
            hover["result"],
            json!({
                "contents":{"kind":"markdown","value":"```bend\ndef shared(x: U32) -> U32\n```"}
            })
        );
        let mut rename = member_position;
        rename["newName"] = json!("renamed");
        let renamed = client.request("textDocument/rename", rename);
        assert_eq!(
            renamed["result"],
            json!({"changes":{
                (uri):[{"range":{"start":{"line":4,"character":7},"end":{"line":4,"character":13}},"newText":"renamed"}],
                (dep_uri):[{"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":10}},"newText":"renamed"}]
            }})
        );
        client.finish();
    }

    fn b5_b6_hold_pending_disk_read(path: &Path) -> fs::File {
        let path = path.to_path_buf();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let writer = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .must_be("open held snapshot FIFO writer");
            sender.send(writer).must_be("signal pending snapshot read");
        });
        receiver
            .recv_timeout(Duration::from_secs(5))
            .must_be("snapshot staging must open the FIFO reader")
    }

    #[test]
    fn queued_close_clears_imported_diagnostics_before_lower_version_reopen() {
        assert_queued_close_clears_imported_diagnostics("def reopened() -> U32:\n  2\n");
    }

    #[test]
    fn queued_close_clears_imported_diagnostics_when_reopened_with_hole() {
        assert_queued_close_clears_imported_diagnostics("def reopened() -> U32:\n  ?TODO\n");
    }

    fn assert_queued_close_clears_imported_diagnostics(reopened_source: &str) {
        let temp = tempdir().must_be("temporary workspace");
        let root_path = temp.path().join("root.bend");
        let dependency_path = temp.path().join("dep.bend");
        let gate_path = temp.path().join("gate.bend");
        let sentinel_path = temp.path().join("sentinel.bend");
        let root_source = "import ./dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        let gate_source = "def gate() -> U32:\n  0\n";
        let sentinel_source = "def sentinel() -> U32:\n  0\n";
        for (path, source) in [
            (&root_path, root_source),
            (&dependency_path, "BAD\n"),
            (&gate_path, gate_source),
            (&sentinel_path, sentinel_source),
        ] {
            fs::write(path, source).must_be("write imported diagnostics epoch fixture");
        }
        let root_uri = regression_file_uri(&root_path);
        let dependency_uri = regression_file_uri(&dependency_path);
        let gate_uri = regression_file_uri(&gate_path);
        let sentinel_uri = regression_file_uri(&sentinel_path);
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":root_uri,"languageId":"bend","version":10,"text":root_source
            }}),
        );
        assert!(diagnostics_for(
            &mut client,
            &dependency_uri,
            false,
            Duration::from_secs(5)
        ));
        open_clean_document(&mut client, &sentinel_uri, sentinel_source);
        fs::remove_file(&gate_path).must_be("replace unrelated gate with FIFO");
        create_fifo(&gate_path);
        notify_watched_file_change(&mut client, &gate_uri);
        let writer = b5_b6_hold_pending_disk_read(&gate_path);
        client.notify(
            "textDocument/didClose",
            json!({"textDocument":{"uri":root_uri}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":root_uri,"languageId":"bend","version":1,
                "text":reopened_source
            }}),
        );
        let reopened = client.request(
            "textDocument/hover",
            json!({"textDocument":{"uri":root_uri},"position":{"line":0,"character":5}}),
        );
        assert_eq!(
            reopened["result"]["contents"]["value"],
            "```bend\ndef reopened() -> U32\n```"
        );
        assert!(
            diagnostics_for(&mut client, &dependency_uri, true, Duration::from_secs(2)),
            "closed epoch diagnostics must not survive a lower-version reopened buffer"
        );
        finish_watched_disk_read(&mut client, writer, &gate_path, gate_source, &sentinel_uri);
        client.finish();
    }

    #[test]
    fn queued_close_cannot_remove_a_reopened_document() {
        let temp = tempdir().must_be("temporary workspace");
        let target_path = temp.path().join("target.bend");
        let gate_path = temp.path().join("gate.bend");
        let sentinel_path = temp.path().join("sentinel.bend");
        let old_source = "import ./gate.bend as Gate\ndef old_value() -> U32:\n  1\n";
        let reopened_source = "import ./gate.bend as Gate\ndef reopened() -> U32:\n  2\n";
        let gate_source = "def gate() -> U32:\n  0\n";
        let sentinel_source = "def sentinel() -> U32:\n  0\n";
        for (path, source) in [
            (&target_path, old_source),
            (&gate_path, gate_source),
            (&sentinel_path, sentinel_source),
        ] {
            fs::write(path, source).must_be("write close-reopen fixture");
        }
        let target_uri = regression_file_uri(&target_path);
        let gate_uri = regression_file_uri(&gate_path);
        let sentinel_uri = regression_file_uri(&sentinel_path);
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        open_clean_document(&mut client, &target_uri, old_source);
        open_clean_document(&mut client, &sentinel_uri, sentinel_source);
        fs::remove_file(&gate_path).must_be("replace gate with FIFO");
        create_fifo(&gate_path);
        notify_watched_file_change(&mut client, &gate_uri);
        let writer = b5_b6_hold_pending_disk_read(&gate_path);
        client.notify(
            "textDocument/didClose",
            json!({"textDocument":{"uri":target_uri}}),
        );
        client.notify(
            "textDocument/didOpen",
            json!({"textDocument":{
                "uri":target_uri,"languageId":"bend","version":2,"text":reopened_source
            }}),
        );
        let params = json!({
            "textDocument":{"uri":target_uri},"position":{"line":1,"character":5}
        });
        let before = client.request("textDocument/hover", params.clone());
        assert_eq!(
            before["result"]["contents"]["value"], "```bend\ndef reopened() -> U32\n```",
            "reopen must commit while the old close is queued behind watcher staging"
        );
        finish_watched_disk_read(&mut client, writer, &gate_path, gate_source, &sentinel_uri);
        let after = client.request("textDocument/hover", params);
        assert_eq!(
            after["result"]["contents"]["value"], "```bend\ndef reopened() -> U32\n```",
            "old queued close must not remove the new open epoch"
        );
        client.finish();
    }

    #[test]
    fn b5_watched_closed_consumer_survives_concurrent_revision_commit() {
        let temp = tempdir().must_be("temporary workspace");
        let main_path = temp.path().join("main.bend");
        let dep_path = temp.path().join("dep.bend");
        let consumer_path = temp.path().join("consumer.bend");
        let sentinel_path = temp.path().join("sentinel.bend");
        let main_source = "import ./dep.bend as Dep\nimport ./consumer.bend as Consumer\ndef main: U32\n  Dep.shared(1)\n";
        let dep_source = "def shared(x: U32) -> U32:\n  x\n";
        let consumer_source = "import ./dep.bend as Dep\ndef consume: U32\n  Dep.shared(2)\n";
        let updated_consumer = format!("{consumer_source}  Dep.shared(3)\n");
        let sentinel_source = "def sentinel: U32\n  1\n";
        for (path, source) in [
            (&main_path, main_source),
            (&dep_path, dep_source),
            (&consumer_path, consumer_source),
            (&sentinel_path, sentinel_source),
        ] {
            fs::write(path, source).must_be("write watcher fixture");
        }
        let main_uri = regression_file_uri(&main_path);
        let dep_uri = regression_file_uri(&dep_path);
        let consumer_uri = regression_file_uri(&consumer_path);
        let sentinel_uri = regression_file_uri(&sentinel_path);
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        open_clean_document(&mut client, &main_uri, main_source);
        open_clean_document(&mut client, &sentinel_uri, sentinel_source);
        let params = json!({
            "textDocument":{"uri":main_uri},
            "position":{"line":3,"character":7},
            "context":{"includeDeclaration":true}
        });
        let before = client.request("textDocument/references", params.clone());
        assert_eq!(
            before["result"]
                .as_array()
                .must_be("initial reference locations")
                .len(),
            3
        );

        fs::remove_file(&consumer_path).must_be("replace watched consumer with a FIFO");
        create_fifo(&consumer_path);
        notify_watched_file_change(&mut client, &consumer_uri);
        // Opening the writer proves the watched snapshot read has started.
        // Keeping it open holds cold staging before its commit without sleeps.
        let writer = b5_b6_hold_pending_disk_read(&consumer_path);
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":main_uri,"version":2},
                "contentChanges":[{"text":main_source}]
            }),
        );
        client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":main_uri}}),
        );
        // Closing a separate root queues behind the watched update's existing
        // serial commit boundary, even when that update is incorrectly dropped.
        finish_watched_disk_read(
            &mut client,
            writer,
            &consumer_path,
            &updated_consumer,
            &sentinel_uri,
        );

        let references = client.request("textDocument/references", params);
        let locations = references["result"]
            .as_array()
            .must_be("updated reference locations");
        let expected = [
            json!({"uri":dep_uri,"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":10}}}),
            json!({"uri":main_uri,"range":{"start":{"line":3,"character":6},"end":{"line":3,"character":12}}}),
            json!({"uri":consumer_uri,"range":{"start":{"line":2,"character":6},"end":{"line":2,"character":12}}}),
            json!({"uri":consumer_uri,"range":{"start":{"line":3,"character":6},"end":{"line":3,"character":12}}}),
        ];
        assert_reference_locations(locations, &expected);
        let rename = client.request("textDocument/rename", json!({
            "textDocument":{"uri":main_uri},"position":{"line":3,"character":7},"newName":"renamed"
        }));
        assert_eq!(
            rename["result"],
            json!({"changes":{
                (dep_uri):[
                    {"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":10}},"newText":"renamed"}
                ],
                (main_uri):[
                    {"range":{"start":{"line":3,"character":6},"end":{"line":3,"character":12}},"newText":"renamed"}
                ],
                (consumer_uri):[
                    {"range":{"start":{"line":2,"character":6},"end":{"line":2,"character":12}},"newText":"renamed"},
                    {"range":{"start":{"line":3,"character":6},"end":{"line":3,"character":12}},"newText":"renamed"}
                ]
            }})
        );
        client.finish();
    }

    #[test]
    fn b6_immediate_definition_waits_for_unsaved_dependency_without_blocking_unrelated_queries() {
        let temp = tempdir().must_be("temporary workspace");
        let main_path = temp.path().join("main.bend");
        let dep_path = temp.path().join("dep.bend");
        let gate_path = temp.path().join("gate.bend");
        let unrelated_path = temp.path().join("unrelated.bend");
        let main_source = "import ./dep.bend as Dep\ndef main: U32\n  Dep.clamp(1)\n";
        let dep_source = "def clamp(x: U32) -> U32:\n  x\n";
        let unrelated_source = "def stable: U32\n  1\ndef unrelated: U32\n  stable\n";
        for (path, source) in [
            (&main_path, main_source),
            (&dep_path, dep_source),
            (&unrelated_path, unrelated_source),
        ] {
            fs::write(path, source).must_be("write unsaved dependency fixture");
        }
        create_fifo(&gate_path);
        let main_uri = Url::from_file_path(&main_path)
            .must_be("main URI")
            .to_string();
        let dep_uri = Url::from_file_path(&dep_path)
            .must_be("dependency URI")
            .to_string();
        let unrelated_uri = Url::from_file_path(&unrelated_path)
            .must_be("unrelated URI")
            .to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        open_clean_document(&mut client, &main_uri, main_source);
        open_clean_document(&mut client, &dep_uri, dep_source);
        open_clean_document(&mut client, &unrelated_uri, unrelated_source);
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":dep_uri,"version":2},
                "contentChanges":[{"text":format!("import ./gate.bend as Gate\n{dep_source}")}]
            }),
        );
        // v2 has committed its snapshot but holds its stage ticket while
        // loading Gate. v3 must remain pending behind that ticket.
        let mut writer = b5_b6_hold_pending_disk_read(&gate_path);
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":dep_uri,"version":3},
                "contentChanges":[{"text":format!("# latest unsaved declaration\nimport ./gate.bend as Gate\n{dep_source}")}]
            }),
        );
        let params = json!({"textDocument":{"uri":main_uri},"position":{"line":2,"character":8}});
        let immediate_id = client.send_request("textDocument/definition", params.clone());
        let unrelated = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":unrelated_uri},"position":{"line":3,"character":3}
            }),
        );
        assert_eq!(
            unrelated["result"],
            json!({
                "uri":unrelated_uri,
                "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":10}}
            }),
            "an unrelated committed query must complete while dependency staging is held"
        );
        writer
            .write_all(b"def gate: U32\n  1\n")
            .must_be("release dependency import snapshot");
        drop(writer);
        fs::remove_file(&gate_path).must_be("remove released dependency FIFO");
        fs::write(&gate_path, "def gate: U32\n  1\n").must_be("restore regular imported file");
        let expected = json!({
            "uri":dep_uri,
            "range":{"start":{"line":2,"character":4},"end":{"line":2,"character":9}}
        });
        let immediate = client
            .receive_matching(Duration::from_secs(10), |message| {
                message["id"] == immediate_id
            })
            .must_be("immediate cross-file definition response");
        assert!(
            immediate.get("error").is_none(),
            "definition failed: {immediate}"
        );
        assert_eq!(
            immediate["result"], expected,
            "immediate request must not observe the preceding dependency snapshot"
        );
        let settled = client.request("textDocument/definition", params);
        assert_eq!(settled["result"], expected);
        client.finish();
    }

    #[test]
    fn b5_watched_disk_commit_preserves_new_open_imports_and_close_fallback() {
        let temp = tempdir().must_be("temporary workspace");
        let main_path = temp.path().join("main.bend");
        let consumer_path = temp.path().join("consumer.bend");
        let first_path = temp.path().join("first.bend");
        let second_path = temp.path().join("second.bend");
        let third_path = temp.path().join("third.bend");
        let sentinel_path = temp.path().join("sentinel.bend");
        let main_source = "import ./consumer.bend as Consumer\nimport ./third.bend as Third\ndef main: U32\n  Third.shared(1)\n";
        let initial_consumer = "import ./first.bend as Dep\ndef consume: U32\n  Dep.shared(2)\n";
        let open_consumer = "import ./second.bend as Dep\ndef consume: U32\n  Dep.shared(2)\n";
        let disk_consumer = "import ./third.bend as Dep\ndef consume: U32\n  Dep.shared(2)\n";
        let dep_source = "def shared(x: U32) -> U32:\n  x\n";
        let sentinel_source = "def sentinel: U32\n  1\n";
        for (path, source) in [
            (&main_path, main_source),
            (&consumer_path, initial_consumer),
            (&first_path, dep_source),
            (&second_path, dep_source),
            (&third_path, dep_source),
            (&sentinel_path, sentinel_source),
        ] {
            fs::write(path, source).must_be("write open-overlay watcher fixture");
        }
        let main_uri = regression_file_uri(&main_path);
        let consumer_uri = regression_file_uri(&consumer_path);
        let second_uri = regression_file_uri(&second_path);
        let third_uri = regression_file_uri(&third_path);
        let sentinel_uri = regression_file_uri(&sentinel_path);
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        open_clean_document(&mut client, &main_uri, main_source);
        open_clean_document(&mut client, &consumer_uri, initial_consumer);
        open_clean_document(&mut client, &sentinel_uri, sentinel_source);

        fs::remove_file(&consumer_path).must_be("replace open consumer disk with a FIFO");
        create_fifo(&consumer_path);
        notify_watched_file_change(&mut client, &consumer_uri);
        let writer = b5_b6_hold_pending_disk_read(&consumer_path);
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":consumer_uri,"version":2},
                "contentChanges":[{"text":open_consumer}]
            }),
        );
        client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":consumer_uri}}),
        );
        finish_watched_disk_read(
            &mut client,
            writer,
            &consumer_path,
            disk_consumer,
            &sentinel_uri,
        );
        let open_definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument":{"uri":consumer_uri},"position":{"line":2,"character":7}
            }),
        );
        assert_eq!(
            open_definition["result"],
            json!({
                "uri":second_uri,
                "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":10}}
            }),
            "a watched disk commit must not replace newer open-overlay import edges"
        );

        client.notify(
            "textDocument/didClose",
            json!({"textDocument":{"uri":consumer_uri}}),
        );
        client
            .receive_matching(Duration::from_secs(5), |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == consumer_uri
                    && message["params"]["version"].is_null()
            })
            .must_be("consumer close must activate its disk snapshot");
        let disk_references = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":main_uri},"position":{"line":3,"character":9},
                "context":{"includeDeclaration":true}
            }),
        );
        let locations = disk_references["result"]
            .as_array()
            .must_be("disk-fallback reference locations");
        let expected = [
            json!({"uri":third_uri,"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":10}}}),
            json!({"uri":main_uri,"range":{"start":{"line":3,"character":8},"end":{"line":3,"character":14}}}),
            json!({"uri":consumer_uri,"range":{"start":{"line":2,"character":6},"end":{"line":2,"character":12}}}),
        ];
        assert_reference_locations(locations, &expected);
        client.finish();
    }

    #[test]
    fn parameter_annotation_navigation_keeps_nested_type_boundaries() {
        let temp = tempdir().must_be("temporary workspace");
        let path = temp.path().join("main.bend");
        let source = "type Hidden:\n  One\ndef typed(simple: U32, nested: (Hidden, List(U32)), tail: Hidden):\n  simple\n  nested\n  tail\n";
        fs::write(&path, source).must_be("write annotation source");
        let uri = regression_file_uri(&path);
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = spawn_client(&compiler_dir);
        client.initialize(temp.path());
        open_clean_document(&mut client, &uri, source);
        for (line, name, annotation) in [
            (3, "simple", "U32"),
            (4, "nested", "(Hidden, List(U32))"),
            (5, "tail", "Hidden"),
        ] {
            let hover = client.request(
                "textDocument/hover",
                json!({"textDocument":{"uri":uri},"position":{"line":line,"character":3}}),
            );
            assert_eq!(
                hover["result"],
                json!({"contents":{"kind":"markdown","value":format!("```bend\n{name}: {annotation}\n```")}}),
            );
        }
        let definition = client.request(
            "textDocument/typeDefinition",
            json!({"textDocument":{"uri":uri},"position":{"line":4,"character":3}}),
        );
        assert_eq!(
            definition["result"],
            json!({"uri":uri,"range":{"start":{"line":0,"character":5},"end":{"line":0,"character":11}}}),
        );
        client.finish();
    }

    fn base_reload_request(client: &mut LspClient, method: &str, uri: &str, line: u32) -> Value {
        client.request(
            method,
            json!({"textDocument":{"uri":uri},"position":{"line":line,"character":7}}),
        )
    }

    fn base_reload_targets(client: &mut LspClient, caller: &Value) -> Value {
        let response = client.request("callHierarchy/outgoingCalls", json!({"item":caller}));
        Value::Array(
            response["result"]
                .as_array()
                .must_be("outgoing Base calls")
                .iter()
                .map(|call| json!({"uri":call["to"]["uri"],"name":call["to"]["name"]}))
                .collect(),
        )
    }

    fn base_reload_references(client: &mut LspClient, uri: &str) -> Value {
        let response = client.request(
            "textDocument/references",
            json!({
                "textDocument":{"uri":uri},
                "position":{"line":0,"character":7},
                "context":{"includeDeclaration":false}
            }),
        );
        Value::Array(
            response["result"]
                .as_array()
                .must_be("Base declaration references")
                .iter()
                .map(|location| {
                    json!({
                        "uri":location["uri"],
                        "line":location["range"]["start"]["line"]
                    })
                })
                .collect(),
        )
    }

    fn base_reload_configure(client: &mut LspClient, compiler: &Path, mode: &str, root_uri: &str) {
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{
                "compilerPath":compiler,
                "compilerArguments":[mode]
            }}}),
        );
        // Initial didOpen diagnostics have already been consumed. This next root
        // publication is scheduled only after configuration's Base reload returns.
        assert_eq!(
            generated_base_diagnostics(client, root_uri, 1)["params"]["diagnostics"],
            json!([]),
        );
        // The request also takes the public workspace readiness/update barrier.
        client.request("workspace/symbol", json!({"query":"main"}));
    }

    const BASE_RELOAD_OLD_SOURCE: &str =
        "def library_value() -> U32:\n  7\ndef old_only() -> U32:\n  1\n";
    const BASE_RELOAD_CURRENT_SOURCE: &str =
        "def library_value() -> U32:\n  9\ndef current_only() -> U32:\n  2\n";

    fn base_reload_item(client: &mut LspClient, uri: &str, line: u32) -> Value {
        base_reload_request(client, "textDocument/prepareCallHierarchy", uri, line)["result"][0]
            .clone()
    }

    struct BaseReloadFixture {
        client: LspClient,
        compiler: PathBuf,
        root_uri: String,
        old_uri: String,
        old_path: PathBuf,
        old_item: Value,
        caller_items: [Value; 3],
        _temp: tempfile::TempDir,
    }

    impl BaseReloadFixture {
        fn new() -> Self {
            let temp = tempdir().must_be("temporary workspace");
            let workspace = temp.path().join("workspace");
            fs::create_dir_all(&workspace).must_be("create workspace");
            let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
            let compiler = compiler_dir.join("bend");
            fs::write(
                &compiler,
                "#!/bin/sh\nmode=old\ncase \"$1\" in --fail|--empty|--current) mode=\"$1\"; shift;; esac\nif [ \"$1\" = base ]; then\n  case \"$mode\" in\n    --fail) exit 1;;\n    --empty) exit 0;;\n    --current) printf 'def library_value() -> U32:\\n  9\\ndef current_only() -> U32:\\n  2\\n';;\n    *) printf 'def library_value() -> U32:\\n  7\\ndef old_only() -> U32:\\n  1\\n';;\n  esac\nfi\nexit 0\n",
            )
            .must_be("write reloadable Base compiler");
            let root_path = workspace.join("main.bend");
            let root_source = "import Base as B\ndef main() -> U32:\n  B.library_value()\ndef previous() -> U32:\n  B.old_only()\ndef current() -> U32:\n  B.current_only()\n";
            fs::write(&root_path, root_source).must_be("write Base consumer");
            let root_uri = regression_file_uri(&root_path);
            let mut client = spawn_client(&compiler_dir);
            client.initialize(&workspace);
            open_clean_document(&mut client, &root_uri, root_source);
            let old_definition =
                base_reload_request(&mut client, "textDocument/definition", &root_uri, 2);
            let old_uri = old_definition["result"]["uri"]
                .as_str()
                .must_be("initial Base URI")
                .to_owned();
            let old_path = Url::parse(&old_uri)
                .must_be("old Base URL")
                .to_file_path()
                .must_be("old Base path");
            assert_eq!(
                fs::read_to_string(&old_path).must_be("read old Base"),
                BASE_RELOAD_OLD_SOURCE,
            );
            open_clean_document(&mut client, &old_uri, BASE_RELOAD_OLD_SOURCE);
            let old_item = base_reload_item(&mut client, &old_uri, 0);
            let caller_items = [1, 3, 5].map(|line| base_reload_item(&mut client, &root_uri, line));
            Self {
                client,
                compiler,
                root_uri,
                old_uri,
                old_path,
                old_item,
                caller_items,
                _temp: temp,
            }
        }

        fn assert_initial_binding(&mut self) {
            assert_eq!(
                base_reload_targets(&mut self.client, &self.caller_items[0]),
                json!([{"uri":self.old_uri,"name":"library_value"}]),
            );
            assert_eq!(
                base_reload_targets(&mut self.client, &self.caller_items[1]),
                json!([{"uri":self.old_uri,"name":"old_only"}]),
            );
            assert_eq!(
                base_reload_targets(&mut self.client, &self.caller_items[2]),
                json!([]),
            );
            assert_eq!(
                base_reload_references(&mut self.client, &self.old_uri),
                json!([{"uri":self.root_uri,"line":2}]),
            );
        }

        fn assert_detached(&mut self, mode: &str) {
            base_reload_configure(&mut self.client, &self.compiler, mode, &self.root_uri);
            for line in [2, 4, 6] {
                for method in [
                    "textDocument/definition",
                    "textDocument/prepareCallHierarchy",
                ] {
                    assert_eq!(
                        base_reload_request(&mut self.client, method, &self.root_uri, line)["result"],
                        Value::Null,
                        "{mode} must not resolve a retired Base through {method}",
                    );
                }
                let references = self.client.request(
                    "textDocument/references",
                    json!({"textDocument":{"uri":self.root_uri},"position":{"line":line,"character":7},"context":{"includeDeclaration":false}}),
                );
                assert_eq!(references["result"], Value::Null);
            }
            for caller in &self.caller_items {
                assert_eq!(base_reload_targets(&mut self.client, caller), json!([]));
            }
            assert_eq!(
                base_reload_references(&mut self.client, &self.old_uri),
                json!([])
            );
            let incoming = self
                .client
                .request("callHierarchy/incomingCalls", json!({"item":self.old_item}));
            assert_eq!(incoming["result"], json!([]));
            assert_eq!(
                fs::read_to_string(&self.old_path).must_be("read retained old Base"),
                BASE_RELOAD_OLD_SOURCE,
            );
            assert_eq!(
                base_reload_request(
                    &mut self.client,
                    "textDocument/definition",
                    &self.old_uri,
                    0
                )["result"]["uri"],
                self.old_uri,
                "retained old source still supports its own local navigation",
            );
        }

        fn assert_recovered(&mut self) {
            base_reload_configure(
                &mut self.client,
                &self.compiler,
                "--current",
                &self.root_uri,
            );
            let recovered = base_reload_request(
                &mut self.client,
                "textDocument/definition",
                &self.root_uri,
                2,
            );
            let current_uri = recovered["result"]["uri"]
                .as_str()
                .must_be("recovered current Base URI")
                .to_owned();
            assert_ne!(current_uri, self.old_uri);
            let current_path = Url::parse(&current_uri)
                .must_be("current Base URL")
                .to_file_path()
                .must_be("current Base path");
            assert_eq!(
                fs::read_to_string(current_path).must_be("read current Base"),
                BASE_RELOAD_CURRENT_SOURCE,
            );
            assert_eq!(
                base_reload_targets(&mut self.client, &self.caller_items[0]),
                json!([{"uri":current_uri,"name":"library_value"}]),
            );
            assert_eq!(
                base_reload_targets(&mut self.client, &self.caller_items[1]),
                json!([]),
            );
            assert_eq!(
                base_reload_targets(&mut self.client, &self.caller_items[2]),
                json!([{"uri":current_uri,"name":"current_only"}]),
            );
            assert_eq!(
                base_reload_request(
                    &mut self.client,
                    "textDocument/definition",
                    &self.root_uri,
                    4
                )["result"],
                Value::Null,
                "a declaration unique to the old source must not survive recovery",
            );
            let prepared = base_reload_request(
                &mut self.client,
                "textDocument/prepareCallHierarchy",
                &self.root_uri,
                6,
            );
            assert_eq!(prepared["result"][0]["uri"], current_uri);
            assert_eq!(prepared["result"][0]["name"], "current_only");
            assert_eq!(
                base_reload_references(&mut self.client, &self.old_uri),
                json!([])
            );
            open_clean_document(&mut self.client, &current_uri, BASE_RELOAD_CURRENT_SOURCE);
            assert_eq!(
                base_reload_references(&mut self.client, &current_uri),
                json!([{"uri":self.root_uri,"line":2}]),
            );
            assert_eq!(
                fs::read_to_string(&self.old_path).must_be("old URI survives recovery"),
                BASE_RELOAD_OLD_SOURCE,
            );
        }
    }

    #[test]
    fn base_reload_failure_detaches_active_index_and_recovers_without_retiring_old_source() {
        let mut fixture = BaseReloadFixture::new();
        fixture.assert_initial_binding();
        // Both nonzero exit and successful-but-empty output must detach the
        // currently bound compiler document, not just the compiler's cache.
        for mode in ["--fail", "--empty"] {
            fixture.assert_detached(mode);
            fixture.assert_recovered();
        }
        fixture.client.finish();
    }
}
