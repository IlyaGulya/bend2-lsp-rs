fn fixture_send(message: &Value) -> ToolResult<()> {
    let body = serde_json::to_vec(message)?;
    let mut stdout = io::stdout().lock();
    write!(stdout, "Content-Length: {}\r\n\r\n", body.len())?;
    stdout.write_all(&body)?;
    stdout.flush()?;
    Ok(())
}

fn fixture_receive(reader: &mut impl BufRead) -> ToolResult<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 { return Err("child received truncated header".into()); }
        if line == "\r\n" { break; }
        let (name, value) = line.split_once(':').ok_or("child received invalid header")?;
        if name.eq_ignore_ascii_case("Content-Length") { length = Some(value.trim().parse::<usize>()?); }
    }
    let mut body = vec![0; length.ok_or("child received no Content-Length")?];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

fn fixture_shutdown(reader: &mut impl BufRead, trailing: Option<&[u8]>) -> ToolResult<()> {
    let shutdown = fixture_receive(reader)?;
    assert_eq!(shutdown["method"], "shutdown");
    fixture_send(&json!({"jsonrpc":"2.0","id":shutdown["id"],"result":null}))?;
    assert_eq!(fixture_receive(reader)?["method"], "exit");
    if let Some(trailing) = trailing { io::stdout().write_all(trailing)?; io::stdout().flush()?; }
    Ok(())
}

pub(crate) fn fixture_child(scenario: &str) -> ToolResult<()> {
    let mut stdin = BufReader::new(io::stdin());
    match scenario {
        "large" => {
            let first = fixture_receive(&mut stdin)?;
            assert_eq!(first, json!({"jsonrpc":"2.0","method":"fixture","params":{"text":"λabc".repeat(512 * 1024)}}));
            let boundary = fixture_receive(&mut stdin)?;
            assert_eq!(boundary["method"], "boundary");
            fixture_send(&json!({"jsonrpc":"2.0","id":boundary["id"],"result":{"verified":true}}))?;
            fixture_shutdown(&mut stdin, None)
        }
        "closed" => fixture_send(&json!({"jsonrpc":"2.0","method":"stdin-closed"})),
        "blocked" => {
            fixture_send(&json!({"jsonrpc":"2.0","method":"not-reading"}))?;
            loop { thread::sleep(Duration::from_secs(60)); }
        }
        "finalization" | "finalization-blocked" | "finalization-invalid" => {
            fixture_send(&json!({"jsonrpc":"2.0","method":"ready-to-shutdown"}))?;
            fixture_shutdown(&mut stdin, None)?;
            let mut remaining = Vec::new();
            stdin.read_to_end(&mut remaining)?;
            assert!(remaining.is_empty(), "stdin retained data after exit");
            fixture_send(&json!({"jsonrpc":"2.0","method":"finalizing"}))?;
            if scenario == "finalization-invalid" {
                io::stdout().write_all(b"Content-Length: 1\r\n\r\nx")?;
                io::stdout().flush()?;
                loop { thread::sleep(Duration::from_secs(60)); }
            }
            if scenario == "finalization-blocked" {
                loop { thread::sleep(Duration::from_secs(60)); }
            }
            thread::sleep(Duration::from_millis(300));
            eprintln!("finalization-stderr {}", "x".repeat(1024 * 1024));
            eprintln!("finalization-complete");
            Ok(())
        }
        "configuration" => fixture_configuration(&mut stdin),
        "out-of-order" => {
            let first = fixture_receive(&mut stdin)?;
            let second = fixture_receive(&mut stdin)?;
            fixture_send(&json!({"jsonrpc":"2.0","id":second["id"],"result":"second"}))?;
            fixture_send(&json!({"jsonrpc":"2.0","id":first["id"],"result":"first"}))?;
            fixture_shutdown(&mut stdin, None)
        }
        "trailing" => fixture_shutdown(&mut stdin, Some(b"Content-Length: 1\r\n\r\nx")),
        "nonzero" => {
            fixture_shutdown(&mut stdin, None)?;
            Err("intentional nonzero fixture exit".into())
        }
        "bad-shutdown" => {
            let request = fixture_receive(&mut stdin)?;
            fixture_send(&json!({"jsonrpc":"2.0","id":request["id"],"result":false}))
        }
        "stderr" => {
            eprintln!("head-marker {}", "x".repeat(10_000));
            let request = fixture_receive(&mut stdin)?;
            eprintln!("tail-marker");
            fixture_send(&json!({"jsonrpc":"2.0","id":request["id"],"result":true}))?;
            fixture_shutdown(&mut stdin, None)
        }
        _ => fixture_invalid(scenario, &mut stdin),
    }
}

fn fixture_configuration(stdin: &mut impl BufRead) -> ToolResult<()> {
    let workspace = std::env::current_dir()?;
    let path = PathBuf::from(std::env::var_os("PATH").ok_or("missing isolated PATH")?);
    assert_eq!(path.canonicalize()?, workspace.join("empty-path").canonicalize()?);
    for key in ["HOME", "USERPROFILE"] {
        let path = PathBuf::from(std::env::var_os(key).ok_or("missing isolated home")?);
        assert_eq!(path.canonicalize()?, workspace.join("empty-home").canonicalize()?);
    }
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().to_ascii_uppercase().starts_with("BEND") {
            assert_eq!(key, "BEND_PERF_TRANSPORT_FIXTURE");
        }
    }
    let initialize = fixture_receive(stdin)?;
    assert_eq!(initialize["method"], "initialize");
    fixture_send(&json!({"jsonrpc":"2.0","id":"configuration","method":"workspace/configuration","params":{"items":[{}, {"section":"bend2-lsp"}, {"section":"unknown"}]}}))?;
    let response = fixture_receive(stdin)?;
    assert_eq!(response["id"], "configuration");
    assert!(response["result"][0]["bend2-lsp"]["compilerArguments"].as_array().is_some_and(Vec::is_empty));
    assert_eq!(response["result"][1]["compilerArguments"], json!([]));
    assert!(response["result"][2].is_null());
    fixture_send(&json!({"jsonrpc":"2.0","id":initialize["id"],"result":{"capabilities":{}}}))?;
    assert_eq!(fixture_receive(stdin)?["method"], "initialized");
    let config = fixture_receive(stdin)?;
    assert_eq!(config["method"], "workspace/didChangeConfiguration");
    let path = config["params"]["settings"]["bend2-lsp"]["compilerPath"].as_str().ok_or("missing compiler path")?;
    assert!(!Path::new(path).exists());
    let open = fixture_receive(stdin)?;
    assert_eq!(open["method"], "textDocument/didOpen");
    let uri = &open["params"]["textDocument"]["uri"];
    fixture_send(&json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"version":1,"diagnostics":[{"code":"compiler-unavailable","message":format!("cannot run {path}")}]}}))?;
    assert_eq!(fixture_receive(stdin)?["method"], "textDocument/didClose");
    fixture_send(&json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":[]}}))?;
    fixture_shutdown(stdin, None)
}

fn fixture_invalid(scenario: &str, stdin: &mut impl BufRead) -> ToolResult<()> {
    let request = fixture_receive(stdin)?;
    match scenario {
        "error" => fixture_send(&json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"fixture failure"}})),
        "unknown-id" => fixture_send(&json!({"jsonrpc":"2.0","id":999,"result":null})),
        "duplicate" => {
            fixture_send(&json!({"jsonrpc":"2.0","id":request["id"],"result":null}))?;
            fixture_send(&json!({"jsonrpc":"2.0","id":request["id"],"result":null}))?;
            let _ = fixture_receive(stdin)?;
            Ok(())
        }
        "no-result" => fixture_send(&json!({"jsonrpc":"2.0","id":request["id"]})),
        "invalid-diagnostics" => fixture_send(&json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///fixture.bend","diagnostics":[null]}})),
        _ => {
            let bytes = match scenario {
                "duplicate-length" => b"Content-Length: 1\r\nContent-Length: 1\r\n\r\nx".to_vec(),
                "truncated" => b"Content-Length: 20\r\n\r\n{}".to_vec(),
                "oversized-header" => vec![b'x'; 8193],
                "oversized-body" => b"Content-Length: 16777217\r\n\r\n".to_vec(),
                "non-json-number" => {
                    let body = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":NaN}";
                    let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
                    bytes.extend_from_slice(body);
                    bytes
                }
                "invalid-json" => b"Content-Length: 1\r\n\r\nx".to_vec(),
                _ => return Err(format!("unknown fixture scenario: {scenario}").into()),
            };
            io::stdout().write_all(&bytes)?;
            io::stdout().flush()?;
            Ok(())
        }
    }
}

fn fixture_process(scenario: &str) -> ToolResult<(tempfile::TempDir, LspProcess)> {
    let workspace = tempfile::tempdir()?;
    let env = [(OsString::from("BEND_PERF_TRANSPORT_FIXTURE"), OsString::from(scenario))];
    let mut client = LspProcess::spawn(&std::env::current_exe()?, workspace.path(), &env)?;
    client.timeout = Duration::from_secs(10);
    Ok((workspace, client))
}

fn fixture_status(client: &LspProcess, expected: &str) -> ToolResult<()> {
    let (body, _) = client.next_message(Instant::now() + Duration::from_secs(10))?.ok_or("fixture exited before status")?;
    let message: Value = serde_json::from_slice(&body)?;
    assert_eq!(message["method"], expected);
    Ok(())
}

fn large_and_pipeline_contracts() -> ToolResult<()> {
    let (_workspace, mut client) = fixture_process("large")?;
    assert!(client.pid() > 0);
    client.notify("fixture", json!({"text":"λabc".repeat(512 * 1024)}))?;
    assert_eq!(client.request("boundary", Value::Null, None)?.0, json!({"verified":true}));
    client.finish()?;
    assert!(client.reader.is_none() && client.writer.is_none());
    assert!(client.child.try_wait()?.is_some_and(|status| status.success()));
    let (_workspace, mut client) = fixture_process("out-of-order")?;
    let first = client.prepare_request("first", Value::Null)?;
    let second = client.prepare_request("second", Value::Null)?;
    let first = client.send_request(first, None)?;
    let second = client.send_request(second, None)?;
    assert_eq!(client.response(first)?.0, "first");
    assert_eq!(client.response(second)?.0, "second");
    client.finish()
}

fn write_failure_contracts() -> ToolResult<()> {
    let (_workspace, mut client) = fixture_process("closed")?;
    fixture_status(&client, "stdin-closed")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.child.try_wait()?.is_none() {
        assert!(Instant::now() < deadline, "fixture did not close stdin");
        thread::sleep(Duration::from_millis(5));
    }
    for _ in 0..2 { assert!(client.notify("boundary", Value::Null).is_err()); }
    let (_workspace, mut client) = fixture_process("blocked")?;
    fixture_status(&client, "not-reading")?;
    client.timeout = Duration::from_millis(100);
    let started = Instant::now();
    assert!(client.notify("large", json!({"text":"x".repeat(8 * 1024 * 1024)})).is_err());
    assert!(started.elapsed() >= client.timeout);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(client.child.try_wait()?.is_some());
    assert!(client.reader.is_none() && client.writer.is_none());
    assert!(client.writes.is_none());
    assert!(client.notify("boundary", Value::Null).is_err());
    Ok(())
}

fn lifecycle_contracts() -> ToolResult<()> {
    let (workspace, mut client) = fixture_process("configuration")?;
    assert_eq!(client.settings()["bend2-lsp"]["compilerArguments"], json!([]));
    client.initialize()?;
    let uri = file_uri(&workspace.path().join("spaces λ.bend"))?;
    assert!(uri.contains("spaces%20%CE%BB.bend"));
    client.notify("textDocument/didOpen", json!({"textDocument":{"uri":uri,"version":1,"languageId":"bend","text":"def main: U32\n  0\n"}}))?;
    client.wait_diagnostics(&uri, Some(1))?;
    assert_eq!(client.diagnostics(&uri).ok_or("diagnostics not retained")?["version"], 1);
    client.close_document(&uri)?;
    assert!(client.diagnostics(&uri).is_none());
    client.finish()?;
    for scenario in ["trailing", "bad-shutdown", "nonzero"] {
        let (_workspace, mut client) = fixture_process(scenario)?;
        assert!(client.finish().is_err(), "accepted invalid lifecycle: {scenario}");
    }
    let (_workspace, mut client) = fixture_process("blocked")?;
    fixture_status(&client, "not-reading")?;
    client.timeout = Duration::from_millis(100);
    assert!(client.request("probe", Value::Null, None).is_err());
    assert!(client.child.try_wait()?.is_some());
    assert!(client.reader.is_none() && client.writer.is_none());
    Ok(())
}

fn cancellation_contract() -> ToolResult<()> {
    static CANCEL_REQUEST: AtomicBool = AtomicBool::new(false);
    let (_workspace, mut client) = fixture_process("blocked")?;
    fixture_status(&client, "not-reading")?;
    client.set_cancellation_flag(&CANCEL_REQUEST);
    CANCEL_REQUEST.store(true, Ordering::Relaxed);
    let started = Instant::now();
    assert!(client.request("probe", Value::Null, None).is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(client.child.try_wait()?.is_some());
    assert!(client.reader.is_none() && client.writer.is_none());
    Ok(())
}

fn protocol_failure_contracts() -> ToolResult<()> {
    for scenario in ["duplicate-length", "truncated", "oversized-header", "oversized-body", "invalid-json", "non-json-number", "error", "unknown-id", "no-result", "invalid-diagnostics"] {
        let (_workspace, mut client) = fixture_process(scenario)?;
        assert!(client.request("probe", Value::Null, None).is_err(), "accepted invalid output: {scenario}");
    }
    let (_workspace, mut client) = fixture_process("duplicate")?;
    client.request("probe", Value::Null, None)?;
    assert!(client.request("next", Value::Null, None).is_err());
    Ok(())
}

fn finalization_contracts() -> ToolResult<()> {
    let (_workspace, mut client) = fixture_process("finalization")?;
    fixture_status(&client, "ready-to-shutdown")?;
    client.timeout = Duration::from_millis(100);
    client.finish_profiled(Duration::from_secs(10))?;
    let evidence = client.shutdown_evidence();
    assert_eq!(evidence.protocol_timeout_ms, 100);
    assert_eq!(evidence.finalization_timeout_ms, 10_000);
    assert_eq!(evidence.protocol_stage, ShutdownProtocolStage::ExitNotified);
    assert!(evidence.stdin_closed);
    assert!(evidence.stdout_eof);
    assert_eq!(evidence.stdout_frames_drained, 1);
    assert_eq!(evidence.child_exit_code, Some(0));
    assert_eq!(evidence.child_exit_success, Some(true));
    assert!(!evidence.forced_termination);
    assert!(client.reader.is_none() && client.writer.is_none());
    assert!(client.messages.is_none() && client.writes.is_none());
    assert!(client.stderr_tail()?.ends_with("finalization-complete"));

    for profiled in [false, true] {
        let (_workspace, mut client) = fixture_process("finalization-blocked")?;
        fixture_status(&client, "ready-to-shutdown")?;
        client.timeout = Duration::from_millis(100);
        let started = Instant::now();
        let outcome = if profiled {
            client.finish_profiled(Duration::from_millis(100))
        } else {
            client.finish()
        };
        let error = outcome.err().ok_or("unbounded finalization unexpectedly accepted")?;
        assert!(error.to_string().contains("LSP did not exit after shutdown"));
        assert!(started.elapsed() < Duration::from_secs(5), "failure cleanup was not bounded");
        assert_eq!(client.shutdown_evidence().protocol_stage, ShutdownProtocolStage::ExitNotified);
        assert!(client.shutdown_evidence().stdin_closed);
        assert!(client.shutdown_evidence().forced_termination);
        assert!(client.child.try_wait()?.is_some());
        assert!(client.reader.is_none() && client.writer.is_none());
        assert!(client.messages.is_none() && client.writes.is_none());
    }
    let (_workspace, mut client) = fixture_process("finalization-invalid")?;
    fixture_status(&client, "ready-to-shutdown")?;
    let started = Instant::now();
    let error = client
        .finish_profiled(Duration::from_secs(10))
        .err()
        .ok_or("invalid finalization output unexpectedly accepted")?;
    assert!(!error.to_string().contains("LSP did not exit after shutdown"));
    assert!(started.elapsed() < Duration::from_secs(5), "output was not drained while finalizing");
    assert!(client.shutdown_evidence().forced_termination);
    assert!(client.child.try_wait()?.is_some());
    assert!(client.reader.is_none() && client.writer.is_none());
    assert!(client.messages.is_none() && client.writes.is_none());

    let (_workspace, mut client) = fixture_process("blocked")?;
    fixture_status(&client, "not-reading")?;
    client.timeout = Duration::from_millis(100);
    let started = Instant::now();
    assert!(client.finish_profiled(Duration::from_secs(10)).is_err());
    assert!(started.elapsed() < Duration::from_secs(5), "profiling grace extended JSON-RPC timeout");
    assert_eq!(client.shutdown_evidence().protocol_stage, ShutdownProtocolStage::Pending);
    assert!(client.child.try_wait()?.is_some());
    assert!(client.reader.is_none() && client.writer.is_none());
    assert!(client.messages.is_none() && client.writes.is_none());
    Ok(())
}

pub(crate) fn contract_regressions() -> ToolResult<()> {
    large_and_pipeline_contracts()?;
    write_failure_contracts()?;
    lifecycle_contracts()?;
    cancellation_contract()?;
    finalization_contracts()?;
    protocol_failure_contracts()?;
    let (_workspace, mut client) = fixture_process("stderr")?;
    assert_eq!(client.request("probe", Value::Null, None)?.0, true);
    let tail = client.stderr_tail()?;
    assert!(tail.len() <= 8192);
    assert!(tail.ends_with("tail-marker"));
    assert!(!tail.contains("head-marker"));
    client.finish()?;
    println!("portable JSON-RPC transport contracts passed");
    Ok(())
}
