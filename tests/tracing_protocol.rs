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
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        process::{Command, Stdio},
        time::Duration,
    };
    use tempfile::{TempDir, tempdir};
    use url::Url;

    fn spawn_trace_client(
        cwd: &Path,
        trace_path: Option<&Path>,
        metrics_path: Option<&Path>,
    ) -> LspClient {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bend2-lsp"));
        command
            .stderr(Stdio::null())
            .current_dir(cwd)
            .env_remove("BEND2_LSP_TRACE")
            .env_remove("BEND2_LSP_COMPILER_METRICS_FILE")
            .env_remove("BEND_LIB");
        if let Some(trace_path) = trace_path {
            command.env("BEND2_LSP_TRACE", trace_path);
        }
        if let Some(metrics_path) = metrics_path {
            command.env("BEND2_LSP_COMPILER_METRICS_FILE", metrics_path);
        }
        LspClient::spawn(command)
    }

    fn configure_compiler(client: &mut LspClient, compiler: &Path) {
        client.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":{"bend2-lsp":{"compilerPath":compiler.to_string_lossy()}}}),
        );
    }

    fn open_document(client: &mut LspClient, uri: &str, source: &str) {
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

    fn wait_for_diagnostics(client: &mut LspClient, uri: &str) {
        let message = client
            .receive_matching(Duration::from_secs(10), |value| {
                value["method"] == "textDocument/publishDiagnostics"
                    && value["params"]["uri"] == uri
                    && value["params"]["version"] == 1
            })
            .must_be("receive diagnostics for opened revision");
        assert!(
            message["params"]["diagnostics"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "stub compiler and valid source should produce no diagnostics: {message}"
        );
    }

    fn install_compiler(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).must_be("create compiler directory");
        let compiler = dir.join("bend-stub");
        fs::write(&compiler, "#!/bin/sh\nexit 0\n").must_be("write compiler stub");
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755))
            .must_be("make compiler stub executable");
        compiler
    }

    fn run_enabled_session(
        temp: &TempDir,
        trace_path: &Path,
        compiler: &Path,
        document_path: &Path,
        source: &str,
        expected_symbol: &str,
    ) -> String {
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let path = workspace.join(document_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).must_be("create document directory");
        }
        fs::write(&path, source).must_be("write source document");
        let uri = Url::from_file_path(&path)
            .must_be("source document URI")
            .to_string();
        let mut client = spawn_trace_client(temp.path(), Some(trace_path), None);
        client.initialize(&workspace);
        configure_compiler(&mut client, compiler);
        open_document(&mut client, &uri, source);
        wait_for_diagnostics(&mut client, &uri);
        let symbols = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            symbols["result"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["name"] == expected_symbol)),
            "document-symbol protocol response should contain the source declaration: {symbols}"
        );
        client.request(
            "textDocument/hover",
            json!({"textDocument":{"uri":uri},"position":{"line":0,"character":5}}),
        );
        client.finish();
        fs::read_to_string(trace_path).must_be("read flushed trace")
    }

    fn trace_events(raw: &str) -> Vec<Value> {
        serde_json::from_str(raw).must_be("parse Chrome Trace JSON")
    }

    fn has_span(events: &[Value], name: &str, phase: &str) -> bool {
        events
            .iter()
            .any(|event| event["name"] == name && event["ph"] == phase)
    }

    fn assert_span_balanced(events: &[Value], name: &str) {
        let starts = events
            .iter()
            .filter(|event| event["name"] == name && event["ph"] == "b")
            .count();
        let ends = events
            .iter()
            .filter(|event| event["name"] == name && event["ph"] == "e")
            .count();
        assert!(starts > 0, "missing async span begin event for {name}");
        assert_eq!(starts, ends, "unclosed async span events for {name}");
    }

    #[test]
    fn disabled_tracing_preserves_protocol_and_compiler_metrics() {
        let temp = tempdir().must_be("temporary LSP process directory");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let source_path = workspace.join("main.bend");
        let source = "def trace_disabled_symbol: U32\n  1\n";
        fs::write(&source_path, source).must_be("write source document");
        let uri = Url::from_file_path(&source_path)
            .must_be("source document URI")
            .to_string();
        let compiler = install_compiler(&temp.path().join("compiler"));
        let metrics = temp.path().join("compiler-metrics.log");
        let mut client = spawn_trace_client(temp.path(), None, Some(&metrics));
        client.initialize(&workspace);
        configure_compiler(&mut client, &compiler);
        open_document(&mut client, &uri, source);
        wait_for_diagnostics(&mut client, &uri);
        let response = client.request(
            "textDocument/documentSymbol",
            json!({"textDocument":{"uri":uri}}),
        );
        assert!(
            response["result"].as_array().is_some_and(|items| items
                .iter()
                .any(|item| item["name"] == "trace_disabled_symbol")),
            "disabled tracing must preserve LSP responses: {response}"
        );
        client.finish();
        let metrics_text = fs::read_to_string(&metrics).must_be("read compiler metrics");
        assert!(
            metrics_text.contains("BEND2_COMPILER_METRIC "),
            "the existing compiler metrics env var remains independent of tracing"
        );
        let generated_traces = fs::read_dir(temp.path())
            .must_be("list process working directory")
            .filter_map(Result::ok)
            .filter(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.starts_with("trace-") && name.ends_with(".json")
            })
            .count();
        assert_eq!(
            generated_traces, 0,
            "disabled tracing must not create a writer"
        );
    }

    #[test]
    fn enabled_trace_is_complete_chrome_json_with_required_spans() {
        let temp = tempdir().must_be("temporary LSP process directory");
        let trace_path = temp.path().join("trace.json");
        let compiler = install_compiler(&temp.path().join("compiler"));
        let raw = run_enabled_session(
            &temp,
            &trace_path,
            &compiler,
            Path::new("main.bend"),
            "def trace_symbol: U32\n  1\n",
            "trace_symbol",
        );
        let events = trace_events(&raw);
        let required = [
            "lsp.request",
            "document.update",
            "document.wait_revision",
            "snapshot.build",
            "workspace.update",
            "workspace.commit",
            "workspace.query",
            "analysis.query",
            "compiler.check",
            "compiler.stage",
            "compiler.child",
            "diagnostics.publish",
        ];
        for name in required {
            assert!(has_span(&events, name, "b"), "missing {name} span: {raw}");
            assert_span_balanced(&events, name);
        }
        assert!(
            raw.contains("textDocument/hover"),
            "request method field is missing"
        );
        assert!(
            raw.contains("result_count"),
            "result count metadata is missing"
        );
        assert!(
            raw.contains("staged_files"),
            "compiler staging counts are missing"
        );
        assert!(
            raw.contains("staged_bytes"),
            "compiler staging byte count is missing"
        );
    }

    #[test]
    fn trace_excludes_source_uri_and_command_sentinels() {
        let temp = tempdir().must_be("temporary LSP process directory");
        let trace_path = temp.path().join("trace.json");
        let compiler = install_compiler(&temp.path().join("TRACE_COMMAND_SENTINEL"));
        let source = "def TRACE_IDENTIFIER_SENTINEL: U32\n  1\n# TRACE_CONTENT_SENTINEL\n";
        let raw = run_enabled_session(
            &temp,
            &trace_path,
            &compiler,
            Path::new("TRACE_PATH_SENTINEL/TRACE_URI_SENTINEL.bend"),
            source,
            "TRACE_IDENTIFIER_SENTINEL",
        );
        for sentinel in [
            "TRACE_IDENTIFIER_SENTINEL",
            "TRACE_CONTENT_SENTINEL",
            "TRACE_PATH_SENTINEL",
            "TRACE_URI_SENTINEL",
            "TRACE_COMMAND_SENTINEL",
        ] {
            assert!(!raw.contains(sentinel), "trace leaked {sentinel}: {raw}");
        }
        assert!(trace_events(&raw).iter().any(|event| event["ph"] == "b"));
    }

    #[test]
    fn graceful_shutdown_flushes_trace_before_process_exit() {
        let temp = tempdir().must_be("temporary LSP process directory");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let trace_path = temp.path().join("graceful-trace.json");
        let mut client = spawn_trace_client(temp.path(), Some(&trace_path), None);
        client.initialize(&workspace);
        client.request("workspace/symbol", json!({"query":""}));
        client.finish();
        let raw = fs::read_to_string(&trace_path).must_be("read trace after process exit");
        assert!(
            raw.trim_end().ends_with(']'),
            "trace writer did not finalize JSON"
        );
        let events = trace_events(&raw);
        assert!(has_span(&events, "lsp.request", "b"));
        assert_span_balanced(&events, "lsp.request");
    }
    #[test]
    fn sigterm_after_shutdown_finalizes_trace() {
        let temp = tempdir().must_be("temporary LSP process directory");
        let workspace = temp.path().join("workspace");
        fs::create_dir_all(&workspace).must_be("create workspace");
        let trace_path = temp.path().join("terminated-trace.json");
        let mut client = spawn_trace_client(temp.path(), Some(&trace_path), None);
        client.initialize(&workspace);
        client.request("workspace/symbol", json!({"query":""}));
        client.request("shutdown", Value::Null);

        client.sigterm();
        let status = client
            .wait_timeout(Duration::from_secs(2))
            .must_be("SIGTERM must terminate without closing stdin");
        assert!(status.success(), "SIGTERM is graceful shutdown: {status}");

        let raw = fs::read_to_string(&trace_path).must_be("read trace after SIGTERM");
        let events = trace_events(&raw);
        assert!(has_span(&events, "lsp.request", "b"));
        assert_span_balanced(&events, "lsp.request");
    }
}
