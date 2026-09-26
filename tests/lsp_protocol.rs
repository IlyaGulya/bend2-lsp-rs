#[cfg(unix)]
mod protocol {
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        ffi::OsString,
        fs,
        io::{BufRead, BufReader, Read, Write},
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Child, ChildStdin, Command, Stdio},
        sync::mpsc::{self, Receiver},
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };
    use tempfile::tempdir;
    use url::Url;

    struct LspClient {
        child: Child,
        stdin: Option<ChildStdin>,
        messages: Receiver<Value>,
        buffered: VecDeque<Value>,
        reader: Option<JoinHandle<()>>,
        next_id: i64,
    }

    impl LspClient {
        fn spawn(compiler_dir: &Path) -> Self {
            Self::spawn_with_optional_bend_lib(compiler_dir, None)
        }

        fn spawn_with_bend_lib(compiler_dir: &Path, bend_lib: &Path) -> Self {
            Self::spawn_with_optional_bend_lib(compiler_dir, Some(bend_lib))
        }

        fn spawn_with_optional_bend_lib(compiler_dir: &Path, bend_lib: Option<&Path>) -> Self {
            let (sender, messages) = mpsc::channel();
            let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .env("PATH", path_with_prefix(compiler_dir));
            if let Some(bend_lib) = bend_lib {
                command.env("BEND_LIB", bend_lib);
            }
            let mut child = command.spawn().expect("start Bend LSP");
            let stdin = child.stdin.take().expect("LSP stdin");
            let stdout = child.stdout.take().expect("LSP stdout");
            let reader = thread::spawn(move || read_messages(stdout, &sender));
            Self {
                child,
                stdin: Some(stdin),
                messages,
                buffered: VecDeque::new(),
                reader: Some(reader),
                next_id: 1,
            }
        }

        fn send(&mut self, message: &Value) {
            let body = serde_json::to_vec(message).expect("serialize JSON-RPC message");
            let stdin = self.stdin.as_mut().expect("open LSP stdin");
            write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write LSP header");
            stdin.write_all(&body).expect("write LSP body");
            stdin.flush().expect("flush LSP message");
        }

        fn notify(&mut self, method: &str, params: Value) {
            let mut message = json!({"jsonrpc":"2.0", "method":method});
            if !params.is_null() {
                message["params"] = params;
            }
            self.send(&message);
        }

        fn request(&mut self, method: &str, params: Value) -> Value {
            let id = self.next_id;
            self.next_id += 1;
            let mut message = json!({"jsonrpc":"2.0", "id":id, "method":method});
            if !params.is_null() {
                message["params"] = params;
            }
            self.send(&message);
            self.receive_matching(Duration::from_secs(5), |message| message["id"] == id)
                .unwrap_or_else(|| panic!("timed out waiting for {method} response"))
        }

        fn receive_matching(
            &mut self,
            timeout: Duration,
            predicate: impl Fn(&Value) -> bool,
        ) -> Option<Value> {
            if let Some(index) = self.buffered.iter().position(&predicate) {
                return self.buffered.remove(index);
            }
            let deadline = Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return None;
                }
                let Ok(message) = self.messages.recv_timeout(remaining) else {
                    return None;
                };
                if predicate(&message) {
                    return Some(message);
                }
                self.buffered.push_back(message);
            }
        }

        fn initialize(&mut self, root: &Path) {
            let root_uri = Url::from_directory_path(root).expect("workspace URI");
            let response = self.request(
                "initialize",
                json!({
                    "processId":null,
                    "rootUri":root_uri,
                    "capabilities":{},
                    "workspaceFolders":[{"uri":root_uri,"name":"test"}]
                }),
            );
            assert!(response["result"]["capabilities"].is_object());
            self.notify("initialized", json!({}));
        }

        fn diagnostics_for(&mut self, uri: &str, empty: bool, timeout: Duration) -> bool {
            self.receive_matching(timeout, |message| {
                message["method"] == "textDocument/publishDiagnostics"
                    && message["params"]["uri"] == uri
                    && message["params"]["diagnostics"]
                        .as_array()
                        .is_some_and(|items| items.is_empty() == empty)
            })
            .is_some()
        }

        fn finish(mut self) {
            let response = self.request("shutdown", Value::Null);
            assert!(
                response.get("error").is_none(),
                "shutdown failed: {response}"
            );
            self.notify("exit", Value::Null);
            self.stdin.take();
            let status = self.child.wait().expect("wait for LSP exit");
            assert!(status.success(), "LSP exited with {status}");
            if let Some(reader) = self.reader.take() {
                reader.join().expect("join LSP output reader");
            }
        }
    }

    impl Drop for LspClient {
        fn drop(&mut self) {
            self.stdin.take();
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn read_messages(stdout: impl Read, sender: &mpsc::Sender<Value>) {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut content_length = None;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if line == "\r\n" || line == "\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse::<usize>().ok();
                }
            }
            let Some(content_length) = content_length else {
                return;
            };
            let mut body = vec![0; content_length];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let Ok(message) = serde_json::from_slice(&body) else {
                return;
            };
            if sender.send(message).is_err() {
                return;
            }
        }
    }

    fn semantic_token_number(value: &Value) -> u32 {
        u32::try_from(
            value
                .as_u64()
                .expect("semantic token field must be an unsigned integer"),
        )
        .expect("semantic token field must fit the LSP u32 field")
    }

    fn path_with_prefix(prefix: &Path) -> OsString {
        let old = std::env::var_os("PATH").unwrap_or_default();
        let paths = std::iter::once(prefix.to_path_buf()).chain(std::env::split_paths(&old));
        std::env::join_paths(paths).expect("construct test PATH")
    }

    fn install_compiler_stub(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).expect("create stub bin directory");
        let executable = dir.join("bend");
        fs::write(
            &executable,
            "#!/bin/sh\ndep=\"${1%/*}/dep.bend\"\nif grep -q 'dep\\.bend as Dep$' \"$1\" && grep -q '^BAD$' \"$dep\"; then\n  printf 'Error:\\nsynthetic imported error\\nLocation:\\n1>| BAD\\n' >&2\n  exit 1\nfi\nexit 0\n",
        )
        .expect("write compiler stub");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .expect("make compiler stub executable");
        dir.to_path_buf()
    }

    #[test]
    fn fixing_a_closed_import_clears_its_published_diagnostic() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let main_text = "import dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        fs::write(&main_path, main_text).expect("write root source");
        fs::write(&dependency_path, "BAD\n").expect("write broken dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path).unwrap().to_string();
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();

        let mut client = LspClient::spawn(&compiler_dir);
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
        assert!(client.diagnostics_for(&dependency_uri, false, Duration::from_secs(5)));

        fs::write(&dependency_path, "def value: Type\n  1\n").expect("fix dependency on disk");
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":main_uri,"version":2},"contentChanges":[{"text":main_text}]}),
        );
        assert!(
            client.diagnostics_for(&dependency_uri, true, Duration::from_secs(2)),
            "fixing a closed imported file must publish an empty diagnostic list for that URI"
        );
        client.finish();
    }

    #[test]
    fn shared_import_diagnostic_remains_until_every_root_is_clean() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let first_path = workspace.join("first.bend");
        let second_path = workspace.join("second.bend");
        let dependency_path = workspace.join("dep.bend");
        let importing_text = "import dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        let clean_text = "def main: Type\n  1\n";
        fs::write(&first_path, importing_text).expect("write first root");
        fs::write(&second_path, importing_text).expect("write second root");
        fs::write(&dependency_path, "BAD\n").expect("write broken dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let first_uri = Url::from_file_path(&first_path).unwrap().to_string();
        let second_uri = Url::from_file_path(&second_path).unwrap().to_string();
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();

        let mut client = LspClient::spawn(&compiler_dir);
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
        assert!(client.diagnostics_for(&dependency_uri, false, Duration::from_secs(5)));
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
            !client.diagnostics_for(&dependency_uri, true, Duration::from_millis(500)),
            "one clean root must not clear another root's imported diagnostic"
        );

        fs::write(&dependency_path, "def value: Type\n  1\n").expect("fix shared dependency");
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":second_uri,"version":2},"contentChanges":[{"text":importing_text}]}),
        );
        assert!(client.diagnostics_for(&dependency_uri, true, Duration::from_secs(2)));
        client.finish();
    }
    #[test]
    fn document_symbols_expose_top_level_declarations_and_constructors() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ntype Shape is Data:\n  Circle{r: U32}\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();

        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("documentSymbol must return symbols");
        assert!(
            symbols.iter().any(|symbol| symbol["name"] == "add"),
            "missing function symbol: {symbols:?}"
        );
        let shape = symbols
            .iter()
            .find(|symbol| symbol["name"] == "Shape")
            .expect("missing type symbol");
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source =
            "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  add(1, 2)\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
                "position":{"line":3,"character":5}
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
                "position":{"line":3,"character":5}
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "type Shape is Data:\n  Circle{}\ndef consume(item: Shape) -> U32:\n  1\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  ad\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
    fn references_and_rename_cover_declaration_and_call_sites() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def add(x: U32) -> U32:\n  x\ndef main: U32\n  add(1)\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def add(x: U32) -> U32:\n  x\ntype Shape is Data:\n  Circle{}\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main: U32\n  1\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
                        "start":{"line":1,"character":2},
                        "end":{"line":1,"character":3}
                    },
                    "text":"2"
                }]
            }),
        );
        let response = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            response["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == "main")),
            "incremental edits must retain untouched declarations: {response}"
        );
        client.finish();
    }
    #[test]
    fn exit_notification_terminates_server_without_stdin_eof() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = LspClient::spawn(&compiler_dir);
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
            if client.child.try_wait().expect("poll LSP process").is_some() {
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let source = "def value: U32\n  1\ndef main: U32\n  val\n";
        let uri = "untitled:scratch.bend";
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source =
            "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  add(1, 2)\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def echo(value: U32) -> String:\n  \"🦀\" # comment\n  42\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("semantic token data");
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main: U32\n  (1 # trailing comment\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let source = "import ./dep.bend as Dep\ndef main: U32\n  Dep.value\n";
        fs::write(&main_path, source).expect("write source");
        fs::write(&dependency_path, "def value: U32\n  1\n").expect("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def first : U32:\n    1\ndef second: U32\n    2\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source =
            "def add(x: U32, y: U32) -> U32:\n  (x + y : U32)\ndef main: U32\n  add(1, 2)\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "type Shape is Data:\n  Circle{}\ndef consume(item: Shape) -> U32:\n  item\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let source = "import Base # builtin prelude\nimport ./dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        fs::write(&main_path, source).expect("write source");
        fs::write(&dependency_path, "def value: Type\n  1\n").expect("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path).unwrap().to_string();
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        assert!(client.diagnostics_for(&main_uri, true, Duration::from_secs(5)));
        fs::write(&dependency_path, "BAD\n").expect("break dependency");
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":dependency_uri,"type":2}]}),
        );
        assert!(
            client.diagnostics_for(&dependency_uri, false, Duration::from_secs(5)),
            "a watched imported-file change must recheck and publish diagnostics: {dependency_uri}"
        );
        client.finish();
    }
    #[test]
    fn advertises_workspace_folders_and_registers_bend_file_watcher() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let root_uri = Url::from_directory_path(&workspace).unwrap();
        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("server should register Bend file watching");
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main: Type\n  1\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let custom_compiler = temp.path().join("configured-bend");
        fs::write(
            &custom_compiler,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"--report-error\" ]; then\n    printf 'Error:\\nconfigured compiler error\\nLocation:\\n1>| def main: Type\\n' >&2\n    exit 1\n  fi\ndone\nexit 0\n",
        )
        .expect("write custom compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .expect("make custom compiler executable");
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        assert!(client.diagnostics_for(&uri, true, Duration::from_secs(5)));
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
        client.finish();
    }
    #[test]
    fn missing_configured_compiler_publishes_actionable_diagnostic() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let source = "def main: Type\n  1\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let missing_compiler = workspace.join("missing-bend");
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("missing compiler should be visible to the editor");
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let dependency_path = workspace.join("dep.bend");
        fs::write(&dependency_path, "def value: Type\n  1\n").expect("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = "untitled:scratch.bend";
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();
        let source =
            "import Base # prelude\nimport ./dep.bend as Dep\ndef main: Type\n  Dep.value\n";
        let mut client = LspClient::spawn(&compiler_dir);
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
        assert!(client.diagnostics_for(uri, true, Duration::from_secs(5)));
        fs::write(&dependency_path, "BAD\n").expect("break dependency");
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":dependency_uri,"type":2}]}),
        );
        assert!(
            client.diagnostics_for(&dependency_uri, false, Duration::from_secs(5)),
            "untitled importers must be rechecked when workspace files change"
        );
        assert!(
            fs::read_dir(&workspace).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".bend2-lsp-virtual-")),
            "virtual source staging must not create files in the workspace"
        );
        client.finish();
    }
    #[test]
    fn workspace_folder_removal_updates_untitled_import_root() {
        let temp = tempdir().expect("temporary workspace");
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        fs::create_dir_all(&first_root).expect("create first workspace root");
        fs::create_dir_all(&second_root).expect("create second workspace root");
        let dependency_path = second_root.join("dep.bend");
        fs::write(&dependency_path, "def value: Type\n  1\n").expect("write dependency");
        let first_uri = Url::from_directory_path(&first_root).unwrap();
        let second_uri = Url::from_directory_path(&second_root).unwrap();
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let custom_compiler = compiler_dir.join("bend");
        fs::write(
            &custom_compiler,
            "#!/bin/sh\nif [ \"$1\" = base ]; then\n  printf 'type Builtin is Data:\\n  Builtin{}\\ndef List.map(x: U32) -> U32:\\n  x\\n'\n  exit 0\nfi\nexit 0\n",
        )
        .expect("write compiler with Base source");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .expect("make compiler executable");
        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("Base definition URI")
            .to_owned();
        assert!(
            fs::read_to_string(Url::parse(&base_uri).unwrap().to_file_path().unwrap())
                .expect("read Base definition source")
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

    #[test]
    fn cached_hub_imports_support_navigation_and_workspace_indexing() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        let bend_lib = temp.path().join("bend-lib");
        let hash = "0123456789abcdef0123456789abcdef";
        let package = bend_lib.join(format!("0x{hash}"));
        let names = bend_lib.join("names");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&package).expect("create cached Hub package");
        fs::create_dir_all(&names).expect("create named-package cache");
        let dependency_path = package.join("main.bend");
        fs::write(&dependency_path, "def exported(arg: U32) -> U32:\n  arg\n")
            .expect("write cached Hub module");
        fs::write(names.join("sample@1.0.0.0"), format!("0x{hash}\n"))
            .expect("write cached Hub name mapping");
        let main_path = workspace.join("main.bend");
        let source = format!(
            "import 0x{hash}/main.bend as Hash\nimport sample@1.0.0.0/main.bend as P\ndef main() -> U32:\n  P.exported(1)\n"
        );
        fs::write(&main_path, &source).expect("write root source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path).unwrap().to_string();
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();
        let mut client = LspClient::spawn_with_bend_lib(&compiler_dir, &bend_lib);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let source = "import Base # prelude\nimport ./dep.bend as Dep\ndef main() -> U32:\n  Dep.exported(1)\n";
        fs::write(&main_path, source).expect("write root source");
        fs::write(&dependency_path, "def exported(arg: U32) -> U32:\n  arg\n")
            .expect("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path).unwrap().to_string();
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
    fn references_and_rename_follow_imports_without_touching_text_or_comments() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let main_source = "import ./dep.bend as Dep\ndef main() -> U32:\n  Dep.value()\n# Dep.value comment\ndef note() -> String:\n  \"Dep.value\"\n";
        let dependency_source = "def value() -> U32:\n  1\ndef local() -> U32:\n  value()\n";
        fs::write(&main_path, main_source).expect("write root source");
        fs::write(&dependency_path, dependency_source).expect("write dependency");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let main_uri = Url::from_file_path(&main_path).unwrap().to_string();
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
    fn type_navigation_resolves_qualified_imported_types() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let types_path = workspace.join("types.bend");
        let source = "import ./types.bend as Types\ndef consume(shape: Types.Shape) -> U32:\n  1\n";
        fs::write(&main_path, source).expect("write source");
        fs::write(&types_path, "type Shape is Data:\n  Circle{}\n").expect("write types");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let types_uri = Url::from_file_path(&types_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let slow_source = "# slow\n\ndef main() -> U32:\n  1\n";
        let current_source = "def main() -> U32:\n  2\n";
        fs::write(&main_path, slow_source).expect("write slow source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let pid_path = temp.path().join("slow-compiler.pid");
        let custom_compiler = temp.path().join("slow-bend");
        fs::write(
            &custom_compiler,
            format!(
                "#!/bin/sh\nif grep -q '^# slow$' \"$1\"; then\n  echo $$ > '{}'\n  exec sleep 30\nfi\nexit 0\n",
                pid_path.display()
            ),
        )
        .expect("write slow compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .expect("make slow compiler executable");
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("slow compiler should have started")
            .trim()
            .to_owned();
        client.notify(
            "textDocument/didChange",
            json!({
                "textDocument":{"uri":uri,"version":2},
                "contentChanges":[{"text":current_source}]
            }),
        );
        assert!(client.diagnostics_for(&uri, true, Duration::from_secs(5)));
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
            "superseded compiler subprocess {pid} should be killed when its version is obsolete"
        );
        client.finish();
    }
    #[test]
    fn closing_document_cancels_running_compiler_process() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let slow_source = "# slow\n\ndef main() -> U32:\n  1\n";
        fs::write(&main_path, slow_source).expect("write slow source");
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
        .expect("write slow compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .expect("make slow compiler executable");
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("slow compiler should have started")
            .trim()
            .to_owned();
        client.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}));
        assert!(
            client.diagnostics_for(&uri, true, Duration::from_secs(5)),
            "closing a document should immediately clear its diagnostics"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut still_running = true;
        while still_running && Instant::now() < deadline {
            still_running = Command::new("/bin/kill")
                .arg("-0")
                .arg(&pid)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        let active_dir = temp.path().join("active");
        fs::create_dir_all(&workspace).expect("create workspace");
        fs::create_dir_all(&active_dir).expect("create compiler activity directory");
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
        )
        .expect("write counting compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .expect("make counting compiler executable");
        let mut client = LspClient::spawn(&compiler_dir);
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
                fs::write(&path, source).expect("write workspace source");
                Url::from_file_path(path).unwrap().to_string()
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
                client.diagnostics_for(uri, true, Duration::from_secs(10)),
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
    fn exit_without_shutdown_cancels_compiler_child_and_keeps_stdin_open() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let slow_source = "# slow\n\ndef main() -> U32:\n  1\n";
        fs::write(&main_path, slow_source).expect("write slow source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let pid_path = temp.path().join("slow-compiler.pid");
        let custom_compiler = temp.path().join("slow-bend");
        fs::write(
            &custom_compiler,
            format!(
                "#!/bin/sh\nif grep -q '^# slow$' \"$1\"; then\n  echo $$ > '{}'\n  exec sleep 30\nfi\nexit 0\n",
                pid_path.display()
            ),
        )
        .expect("write slow compiler");
        fs::set_permissions(&custom_compiler, fs::Permissions::from_mode(0o755))
            .expect("make slow compiler executable");
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("slow compiler should have started")
            .trim()
            .to_owned();
        client.notify("exit", Value::Null);
        let deadline = Instant::now() + Duration::from_secs(2);
        while client.child.try_wait().expect("poll LSP process").is_none()
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            client.child.try_wait().expect("poll LSP process").is_some(),
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
            .unwrap()
            .iter()
            .find(|call| call["to"]["name"] == "external")
            .expect("outgoing calls should include the imported function")["to"]
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
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let dependency_source = "def external(x: U32) -> U32:\n  x\n";
        fs::write(&dependency_path, dependency_source).expect("write dependency");
        let source = "import ./dep.bend as Dep\ntype Shape is Data:\n  Circle{}\n  Square{}\ndef leaf(x: U32) -> U32:\n  x\ndef root(x: U32) -> U32:\n  leaf(x) + Dep.external(x)\ndef shape(x: Shape) -> U32:\n  x\n";
        fs::write(&main_path, source).expect("write source");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
        let initialized = client.request(
            "initialize",
            json!({
                "processId":null,
                "rootUri":Url::from_directory_path(&workspace).unwrap(),
                "workspaceFolders":[{"uri":Url::from_directory_path(&workspace).unwrap(),"name":"test"}],
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
            .expect("server should dynamically register type hierarchy");
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
    fn identical_source_graph_reuses_compiler_result() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let compiler = temp.path().join("counting-bend");
        let calls = temp.path().join("calls");
        fs::write(
            &compiler,
            format!(
                "#!/bin/sh\nprintf 'called\\n' >> '{}'\nexit 0\n",
                calls.display()
            ),
        )
        .expect("write counting compiler");
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .expect("make counting compiler executable");
        let main_path = workspace.join("main.bend");
        let dependency_path = workspace.join("dep.bend");
        let dependency_uri = Url::from_file_path(&dependency_path).unwrap().to_string();
        fs::write(&dependency_path, "def value: U32\n  1\n").expect("write dependency");
        let source = "import ./dep.bend as Dep\ndef main: U32\n  Dep.value\n";
        fs::write(&main_path, source).expect("write source");
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let mut client = LspClient::spawn(&compiler_dir);
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
        assert!(client.diagnostics_for(&uri, true, Duration::from_secs(5)));
        client.notify(
            "textDocument/didChange",
            json!({"textDocument":{"uri":uri,"version":2},"contentChanges":[{"text":source}]}),
        );
        assert!(client.diagnostics_for(&uri, true, Duration::from_secs(5)));
        assert_eq!(
            fs::read_to_string(&calls)
                .expect("compiler invocation log")
                .lines()
                .count(),
            1,
            "unchanged root and dependency sources should reuse the compiler result"
        );
        fs::write(&dependency_path, "def value: U32\n  2\n").expect("update dependency");
        client.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":dependency_uri,"type":2}]}),
        );
        assert!(client.diagnostics_for(&uri, true, Duration::from_secs(5)));
        assert_eq!(
            fs::read_to_string(&calls)
                .expect("compiler invocation log")
                .lines()
                .count(),
            2,
            "changing a closed dependency must invalidate the cached result"
        );
        client.finish();
    }
    #[test]
    fn ambiguous_compiler_excerpt_falls_back_to_root_without_misattributing_import_errors() {
        let temp = tempdir().expect("temporary workspace");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).expect("create workspace");
        let main_path = workspace.join("main.bend");
        let first_dependency = workspace.join("first.bend");
        let second_dependency = workspace.join("second.bend");
        let source = "import ./first.bend as First\nimport ./second.bend as Second\ndef main: U32\n  First.value\n";
        let repeated_excerpt = "def value: U32\n  1\n";
        fs::write(&main_path, source).expect("write root source");
        fs::write(&first_dependency, repeated_excerpt).expect("write first dependency");
        fs::write(&second_dependency, repeated_excerpt).expect("write second dependency");
        let compiler = temp.path().join("ambiguous-bend");
        fs::write(
            &compiler,
            "#!/bin/sh\nprintf 'Error:\\nambiguous compiler error\\nLocation:\\n1>| def value: U32\\n' >&2\nexit 1\n",
        )
        .expect("write compiler");
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .expect("make compiler executable");
        let compiler_dir = install_compiler_stub(&temp.path().join("bin"));
        let uri = Url::from_file_path(&main_path).unwrap().to_string();
        let mut client = LspClient::spawn(&compiler_dir);
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
            .expect("ambiguous excerpt should fall back to a root diagnostic");
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
}
