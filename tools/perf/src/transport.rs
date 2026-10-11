use crate::ToolResult;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

type Received = Result<Option<(Vec<u8>, Instant)>, String>;
type WriteJob = (Vec<u8>, mpsc::Sender<Result<(), String>>);

pub(crate) struct PreparedRequest {
    id: u64,
    method: String,
    frame: Vec<u8>,
}

#[derive(Clone, Copy)]
pub(crate) struct PendingRequest {
    id: u64,
    started: Instant,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ShutdownProtocolStage {
    #[default]
    Pending,
    Acknowledged,
    ExitNotified,
}

#[derive(Default, Serialize)]
pub(crate) struct ShutdownEvidence {
    pub(crate) protocol_timeout_ms: u128,
    pub(crate) finalization_timeout_ms: u128,
    pub(crate) protocol_stage: ShutdownProtocolStage,
    pub(crate) stdin_closed: bool,
    pub(crate) stdout_frames_drained: usize,
    pub(crate) stdout_eof: bool,
    pub(crate) child_exit_code: Option<i32>,
    pub(crate) child_exit_success: Option<bool>,
    pub(crate) finalization_elapsed_ms: u128,
    pub(crate) forced_termination: bool,
}

pub(crate) struct LspProcess {
    child: Child,
    workspace: PathBuf,
    settings: Value,
    compiler_path: String,
    stderr: tempfile::NamedTempFile,
    messages: Option<Receiver<Received>>,
    writes: Option<SyncSender<WriteJob>>,
    reader: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    pending: BTreeMap<u64, String>,
    responses: BTreeMap<u64, (Value, Instant)>,
    diagnostics: BTreeMap<String, Value>,
    next_id: u64,
    timeout: Duration,
    shutdown: ShutdownEvidence,
    cancellation: Option<&'static AtomicBool>,
    abort_on_failure: bool,
}

fn failure(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}

pub(crate) fn file_uri(path: &Path) -> ToolResult<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let uri = url::Url::from_file_path(&absolute)
        .map_err(|()| failure(format!("cannot form file URI for {}", absolute.display())))?;
    Ok(uri.into())
}

fn frame(message: &Value) -> ToolResult<Vec<u8>> {
    let body = serde_json::to_vec(message)?;
    if body.is_empty() || body.len() > MAX_FRAME_BYTES {
        return Err(failure("outgoing LSP frame exceeds limit").into());
    }
    let mut framed = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    framed.extend_from_slice(&body);
    Ok(framed)
}

fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<(Vec<u8>, Instant)>> {
    let mut length = None;
    let mut header_bytes = 0;
    loop {
        let mut line = Vec::new();
        let count = (&mut *reader).take(8193).read_until(b'\n', &mut line)?;
        if count == 0 {
            return if header_bytes == 0 {
                Ok(None)
            } else {
                Err(failure("EOF inside an LSP header"))
            };
        }
        header_bytes += count;
        if count > 8192 || header_bytes > 16384 {
            return Err(failure("oversized LSP header"));
        }
        if line == b"\r\n" {
            break;
        }
        if !line.ends_with(b"\r\n") {
            return Err(failure("invalid LSP header"));
        }
        let header = &line[..line.len() - 2];
        let colon = header
            .iter()
            .position(|byte| *byte == b':')
            .ok_or_else(|| failure("invalid LSP header"))?;
        if header[..colon].eq_ignore_ascii_case(b"content-length") {
            let value = std::str::from_utf8(&header[colon + 1..])
                .map_err(failure)?
                .trim();
            if length.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(failure("invalid or duplicate Content-Length"));
            }
            length = Some(value.parse::<usize>().map_err(failure)?);
        }
    }
    let length = length
        .filter(|length| *length > 0 && *length <= MAX_FRAME_BYTES)
        .ok_or_else(|| failure("missing or invalid LSP Content-Length"))?;
    let mut body = vec![0; length];
    let read = reader.read_exact(&mut body);
    let received = Instant::now();
    read.map_err(|error| failure(format!("EOF or read failure inside an LSP body: {error}")))?;
    Ok(Some((body, received)))
}

fn spawn_reader(
    stdout: impl Read + Send + 'static,
) -> io::Result<(JoinHandle<()>, Receiver<Received>)> {
    let (sender, messages) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("lsp-frame-reader".into())
        .spawn(move || {
            let mut stdout = BufReader::new(stdout);
            loop {
                let result = read_frame(&mut stdout).map_err(|error| error.to_string());
                let done = !matches!(result, Ok(Some(_)));
                if sender.send(result).is_err() || done {
                    break;
                }
            }
        })?;
    Ok((reader, messages))
}

fn spawn_writer(
    mut stdin: impl Write + Send + 'static,
) -> io::Result<(JoinHandle<()>, SyncSender<WriteJob>)> {
    let (commands, jobs) = mpsc::sync_channel::<WriteJob>(1);
    let writer = thread::Builder::new()
        .name("lsp-frame-writer".into())
        .spawn(move || {
            let mut failed: Option<String> = None;
            while let Ok((data, completed)) = jobs.recv() {
                let result = if let Some(error) = &failed {
                    Err(error.clone())
                } else {
                    stdin
                        .write_all(&data)
                        .and_then(|()| stdin.flush())
                        .map_err(|error| error.to_string())
                };
                if let Err(error) = &result {
                    failed = Some(error.clone());
                }
                let _ = completed.send(result);
            }
        })?;
    Ok((writer, commands))
}

impl LspProcess {
    pub(crate) fn spawn(
        binary: &Path,
        workspace: &Path,
        child_env: &[(OsString, OsString)],
    ) -> ToolResult<Self> {
        let workspace = workspace.canonicalize()?;
        let compiler = workspace.join("unavailable-compiler").join("bend");
        let compiler_path = compiler
            .to_str()
            .ok_or_else(|| failure("compiler path is not UTF-8"))?
            .to_owned();
        let settings = json!({"bend2-lsp":{"compilerPath":compiler_path,"compilerArguments":[]}});
        let empty_path = workspace.join("empty-path");
        let home = workspace.join("empty-home");
        fs::create_dir_all(&empty_path)?;
        fs::create_dir_all(&home)?;
        let stderr = tempfile::NamedTempFile::new_in(&workspace)?;
        let mut command = Command::new(binary);
        command
            .current_dir(&workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr.reopen()?);
        for (key, _) in std::env::vars_os() {
            if key
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("BEND")
            {
                command.env_remove(key);
            }
        }
        command
            .env("PATH", empty_path)
            .env("HOME", &home)
            .env("USERPROFILE", home);
        command.envs(child_env.iter().cloned());
        let mut child = command.spawn()?;
        let Some(stdin) = child.stdin.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(failure("LSP stdin unavailable").into());
        };
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err(failure("LSP stdout unavailable").into());
        };
        let (reader, messages) = match spawn_reader(stdout) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        };
        let (writer, commands) = match spawn_writer(stdin) {
            Ok(writer) => writer,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                drop(messages);
                let _ = reader.join();
                return Err(error.into());
            }
        };
        Ok(Self {
            child,
            workspace,
            settings,
            compiler_path,
            stderr,
            messages: Some(messages),
            writes: Some(commands),
            reader: Some(reader),
            writer: Some(writer),
            pending: BTreeMap::new(),
            responses: BTreeMap::new(),
            diagnostics: BTreeMap::new(),
            next_id: 1,
            timeout: REQUEST_TIMEOUT,
            shutdown: ShutdownEvidence::default(),
            cancellation: None,
            abort_on_failure: true,
        })
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn set_cancellation_flag(&mut self, cancellation: &'static AtomicBool) {
        self.cancellation = Some(cancellation);
    }

    /// The scenario owner stops attach collectors before terminating their
    /// target. It must call `abort` on failure; Drop remains the final backstop.
    pub(crate) fn defer_abort_to_owner(&mut self) {
        self.abort_on_failure = false;
    }

    fn abort_failed_operation(&mut self) {
        if self.abort_on_failure {
            self.abort();
        }
    }

    pub(crate) fn check_cancelled(&self) -> ToolResult<()> {
        if self
            .cancellation
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return Err(failure("Performance scenario cancelled").into());
        }
        Ok(())
    }

    fn receive_cancellable<T>(&self, receiver: &Receiver<T>, deadline: Instant) -> ToolResult<T> {
        loop {
            self.check_cancelled()?;
            let remaining = deadline.saturating_duration_since(Instant::now());
            // Existing collectors keep their single blocking wait. Only the
            // shared profile runner opts in to cooperative signal handling.
            let interval = if self.cancellation.is_some() {
                remaining.min(Duration::from_millis(100))
            } else {
                remaining
            };
            match receiver.recv_timeout(interval) {
                Ok(value) => return Ok(value),
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < deadline => {}
                Err(error) => {
                    let kind = match error {
                        mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                        mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::Other,
                    };
                    return Err(
                        io::Error::new(kind, format!("LSP channel wait failed: {error}")).into(),
                    );
                }
            }
        }
    }
    pub(crate) fn settings(&self) -> &Value {
        &self.settings
    }
    pub(crate) fn diagnostics(&self, uri: &str) -> Option<&Value> {
        self.diagnostics.get(uri)
    }

    fn write_frame(&mut self, data: Vec<u8>, deadline: Instant) -> ToolResult<()> {
        if let Err(error) = self.check_cancelled() {
            self.abort_failed_operation();
            return Err(error);
        }
        let (completed, completion) = mpsc::channel();
        self.writes
            .as_ref()
            .ok_or_else(|| failure("LSP stdin is closed"))?
            .try_send((data, completed))
            .map_err(|error| failure(format!("LSP writer unavailable: {error}")))?;
        match self.receive_cancellable(&completion, deadline) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(failure(format!("LSP write failed: {error}")).into()),
            Err(error) => {
                self.abort_failed_operation();
                Err(error)
            }
        }
    }

    pub(crate) fn notify(&mut self, method: &str, params: Value) -> ToolResult<()> {
        let mut message = json!({"jsonrpc":"2.0","method":method});
        if !params.is_null() {
            message["params"] = params;
        }
        self.write_frame(frame(&message)?, Instant::now() + self.timeout)
    }

    pub(crate) fn prepare_request(
        &mut self,
        method: &str,
        params: Value,
    ) -> ToolResult<PreparedRequest> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| failure("LSP request ID exhausted"))?;
        let mut message = json!({"jsonrpc":"2.0","id":id,"method":method});
        if !params.is_null() {
            message["params"] = params;
        }
        Ok(PreparedRequest {
            id,
            method: method.to_owned(),
            frame: frame(&message)?,
        })
    }

    pub(crate) fn send_request(
        &mut self,
        prepared: PreparedRequest,
        notification: Option<Value>,
    ) -> ToolResult<PendingRequest> {
        let notification = notification.map(|message| frame(&message)).transpose()?;
        self.pending.insert(prepared.id, prepared.method);
        let started = Instant::now();
        let deadline = started + self.timeout;
        if let Some(notification) = notification {
            self.write_frame(notification, deadline)?;
        }
        self.write_frame(prepared.frame, deadline)?;
        Ok(PendingRequest {
            id: prepared.id,
            started,
            deadline,
        })
    }

    pub(crate) fn response(&mut self, pending: PendingRequest) -> ToolResult<(Value, u64)> {
        if let Err(error) = self.check_cancelled() {
            self.abort_failed_operation();
            return Err(error);
        }
        while !self.responses.contains_key(&pending.id) {
            self.receive(pending.deadline)?;
        }
        let (result, received) = self
            .responses
            .remove(&pending.id)
            .ok_or_else(|| failure("missing buffered response"))?;
        self.pending.remove(&pending.id);
        if received > pending.deadline {
            self.abort_failed_operation();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "LSP response arrived after request deadline",
            )
            .into());
        }
        let elapsed = u64::try_from(
            received
                .checked_duration_since(pending.started)
                .ok_or_else(|| failure("response preceded request"))?
                .as_nanos(),
        )?;
        if elapsed == 0 {
            return Err(failure("non-positive request duration").into());
        }
        Ok((result, elapsed))
    }

    pub(crate) fn request(
        &mut self,
        method: &str,
        params: Value,
        notification: Option<Value>,
    ) -> ToolResult<(Value, u64)> {
        let prepared = self.prepare_request(method, params)?;
        let pending = self.send_request(prepared, notification)?;
        self.response(pending)
    }

    fn server_request(
        &mut self,
        message: &Value,
        method: &str,
        deadline: Instant,
    ) -> ToolResult<()> {
        let id = &message["id"];
        if !id.is_string() && id.as_i64().is_none() && id.as_u64().is_none() {
            return Err(failure("invalid server request ID").into());
        }
        let result = match method {
            "client/registerCapability"
            | "client/unregisterCapability"
            | "window/workDoneProgress/create"
            | "window/showMessageRequest" => Value::Null,
            "workspace/configuration" => {
                let items = message["params"]["items"]
                    .as_array()
                    .ok_or_else(|| failure("malformed workspace/configuration request"))?;
                let mut results = Vec::with_capacity(items.len());
                for item in items {
                    if !item.is_object() {
                        return Err(failure("malformed workspace/configuration item").into());
                    }
                    results.push(match item.get("section") {
                        None | Some(Value::Null) => self.settings.clone(),
                        Some(Value::String(section)) => {
                            self.settings.get(section).cloned().unwrap_or(Value::Null)
                        }
                        _ => {
                            return Err(failure("malformed workspace/configuration section").into());
                        }
                    });
                }
                Value::Array(results)
            }
            "workspace/workspaceFolders" => {
                json!([{"uri":file_uri(&self.workspace)?,"name":"latency"}])
            }
            _ => {
                self.write_frame(frame(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not supported by latency client"}}))?, deadline)?;
                return Err(failure(format!("unexpected server request: {method}")).into());
            }
        };
        self.write_frame(
            frame(&json!({"jsonrpc":"2.0","id":id,"result":result}))?,
            deadline,
        )
    }

    fn dispatch(&mut self, body: &[u8], received: Instant, deadline: Instant) -> ToolResult<()> {
        let message: Value = serde_json::from_slice(body)?;
        if !message.is_object() || message["jsonrpc"] != "2.0" {
            return Err(failure("invalid JSON-RPC message").into());
        }
        if let Some(method) = message.get("method") {
            let method = method
                .as_str()
                .ok_or_else(|| failure("invalid JSON-RPC method"))?;
            if message.get("result").is_some() || message.get("error").is_some() {
                return Err(failure("invalid JSON-RPC call").into());
            }
            if message.get("id").is_some() {
                return self.server_request(&message, method, deadline);
            }
            if method == "textDocument/publishDiagnostics" {
                let params = &message["params"];
                let uri = params["uri"]
                    .as_str()
                    .ok_or_else(|| failure("malformed diagnostics URI"))?;
                if !params["diagnostics"]
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_object))
                    || params
                        .get("version")
                        .is_some_and(|version| !version.is_null() && version.as_i64().is_none())
                {
                    return Err(failure("malformed publishDiagnostics notification").into());
                }
                self.diagnostics.insert(uri.to_owned(), params.clone());
            }
            return Ok(());
        }
        let id = message["id"]
            .as_u64()
            .ok_or_else(|| failure("invalid LSP response ID"))?;
        let method = self
            .pending
            .get(&id)
            .ok_or_else(|| failure(format!("unexpected LSP response ID: {id}")))?;
        if self.responses.contains_key(&id) {
            return Err(failure(format!("duplicate LSP response ID: {id}")).into());
        }
        if let Some(error) = message.get("error") {
            return Err(failure(format!("{method} failed: {error}")).into());
        }
        let result = message
            .get("result")
            .ok_or_else(|| failure("LSP response has no result"))?;
        self.responses.insert(id, (result.clone(), received));
        Ok(())
    }

    fn next_message(&self, deadline: Instant) -> ToolResult<Option<(Vec<u8>, Instant)>> {
        let receiver = self
            .messages
            .as_ref()
            .ok_or_else(|| failure("LSP stdout is closed"))?;
        let result = self.receive_cancellable(receiver, deadline)?;
        result.map_err(|error| failure(format!("LSP transport failed: {error}")).into())
    }

    fn receive(&mut self, deadline: Instant) -> ToolResult<()> {
        let result = match self.next_message(deadline) {
            Ok(Some((body, received))) => self.dispatch(&body, received, deadline),
            Ok(None) => Err(failure("unexpected EOF from LSP").into()),
            Err(error) => Err(error),
        };
        if result.is_err() {
            self.abort_failed_operation();
        }
        result
    }

    pub(crate) fn initialize(&mut self) -> ToolResult<()> {
        let uri = file_uri(&self.workspace)?;
        let (result, _) = self.request("initialize", json!({"processId":null,"rootUri":uri,"capabilities":{},"workspaceFolders":[{"uri":uri,"name":"latency"}]}), None)?;
        if !result["capabilities"].is_object() {
            return Err(failure("initialize did not return server capabilities").into());
        }
        self.notify("initialized", json!({}))?;
        self.notify(
            "workspace/didChangeConfiguration",
            json!({"settings":self.settings}),
        )
    }

    pub(crate) fn wait_diagnostics(&mut self, uri: &str, version: Option<i64>) -> ToolResult<()> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(params) = self.diagnostics.get(uri)
                && params.get("version").and_then(Value::as_i64) == version
            {
                let items = params["diagnostics"]
                    .as_array()
                    .ok_or_else(|| failure("malformed diagnostics"))?;
                if version.is_none() {
                    if !items.is_empty() {
                        return Err(failure("didClose did not clear diagnostics").into());
                    }
                } else if !items.iter().any(|item| {
                    item["code"] == "compiler-unavailable"
                        && item["message"]
                            .as_str()
                            .is_some_and(|message| message.contains(&self.compiler_path))
                }) {
                    return Err(failure(format!(
                        "revision {version:?} did not confirm unavailable compiler: {params}"
                    ))
                    .into());
                }
                return Ok(());
            }
            self.receive(deadline)?;
        }
    }

    pub(crate) fn close_document(&mut self, uri: &str) -> ToolResult<()> {
        self.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))?;
        self.wait_diagnostics(uri, None)?;
        self.diagnostics.remove(uri);
        Ok(())
    }

    pub(crate) fn finish(&mut self) -> ToolResult<()> {
        self.finish_with_finalization_timeout(self.timeout)
    }

    pub(crate) fn finish_profiled(&mut self, finalization_timeout: Duration) -> ToolResult<()> {
        self.finish_with_finalization_timeout(finalization_timeout)
    }

    pub(crate) fn shutdown_evidence(&self) -> &ShutdownEvidence {
        &self.shutdown
    }

    fn finish_with_finalization_timeout(
        &mut self,
        finalization_timeout: Duration,
    ) -> ToolResult<()> {
        self.shutdown = ShutdownEvidence {
            protocol_timeout_ms: self.timeout.as_millis(),
            finalization_timeout_ms: finalization_timeout.as_millis(),
            ..ShutdownEvidence::default()
        };
        let outcome = self.shutdown_and_exit(finalization_timeout);
        if outcome.is_err() {
            self.abort_failed_operation();
        }
        outcome
    }

    fn shutdown_and_exit(&mut self, finalization_timeout: Duration) -> ToolResult<()> {
        let (result, _) = self.request("shutdown", Value::Null, None)?;
        if !result.is_null() {
            return Err(failure("shutdown returned non-null result").into());
        }
        self.shutdown.protocol_stage = ShutdownProtocolStage::Acknowledged;
        self.notify("exit", Value::Null)?;
        self.shutdown.protocol_stage = ShutdownProtocolStage::ExitNotified;
        self.writes.take();
        if let Some(writer) = self.writer.take() {
            writer.join().map_err(|_| failure("LSP writer panicked"))?;
        }
        self.shutdown.stdin_closed = true;
        let started = Instant::now();
        let outcome = self.drain_until_exit(started + finalization_timeout);
        self.shutdown.finalization_elapsed_ms = started.elapsed().as_millis();
        outcome
    }

    fn drain_until_exit(&mut self, deadline: Instant) -> ToolResult<()> {
        loop {
            self.check_cancelled()?;
            if let Some(status) = self.child.try_wait()? {
                self.shutdown.child_exit_code = status.code();
                self.shutdown.child_exit_success = Some(status.success());
                if !status.success() {
                    return Err(failure(format!("LSP exited with status {status}")).into());
                }
                if self.shutdown.stdout_eof {
                    break;
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let message = if self.shutdown.child_exit_success.is_some() {
                    "timed out draining LSP stdout after shutdown"
                } else {
                    "LSP did not exit after shutdown"
                };
                return Err(failure(message).into());
            }
            let interval = remaining.min(Duration::from_millis(5));
            if self.shutdown.stdout_eof {
                thread::sleep(interval);
                continue;
            }
            let received = self
                .messages
                .as_ref()
                .ok_or_else(|| failure("LSP stdout is closed"))?
                .recv_timeout(interval);
            match received {
                Ok(Ok(Some((body, received)))) => {
                    self.shutdown.stdout_frames_drained += 1;
                    self.dispatch(&body, received, deadline)?;
                }
                Ok(Ok(None)) => self.shutdown.stdout_eof = true,
                Ok(Err(error)) => {
                    return Err(failure(format!("LSP transport failed: {error}")).into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(failure("LSP stdout disconnected without EOF").into());
                }
            }
        }
        if let Some(reader) = self.reader.take() {
            reader.join().map_err(|_| failure("LSP reader panicked"))?;
        }
        self.messages.take();
        Ok(())
    }

    pub(crate) fn stderr_tail(&mut self) -> ToolResult<String> {
        let stderr = self.stderr.as_file_mut();
        let length = stderr.seek(SeekFrom::End(0))?;
        stderr.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
        let mut bytes = vec![0; usize::try_from(length.min(8192))?];
        stderr.read_exact(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
    }

    pub(crate) fn abort(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.shutdown.forced_termination = true;
            let _ = self.child.kill();
        }
        if let Ok(status) = self.child.wait() {
            self.shutdown.child_exit_code = status.code();
            self.shutdown.child_exit_success = Some(status.success());
        }
        self.writes.take();
        self.messages.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        self.abort();
    }
}
